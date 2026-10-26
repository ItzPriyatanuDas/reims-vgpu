//! The video device's backends as its host session reaches them, logged: each decoder's and
//! encoder's opening and end at info, and each frame it decodes or encodes at debug
//! (`reims_vgpu::video`), with the time the backend took on the platform's clock. Host-side
//! evidence that a guest's frames decode and encode where the backend says, VA-API's on Linux.
//!
//! Every method of core's traits is passed on, those with a default too: a default here would
//! answer the session in the backend's place, as one for `frames_decoded` would lend an AV1 unit
//! one buffer a frame output, and one for `session_capabilities` would answer `+enc` with the
//! listed entry while VideoToolbox has no encoder session to give.

use std::sync::Arc;

use reims_vgpu_core::platform::Platform;
use reims_vgpu_core::platform::os::clock::Clock as _;
use reims_vgpu_video_core::backend::{
    BackendDecoder, BackendError, Codec, CodecCapabilities, CompressedFrame, DecodedFrame,
    DecoderBackend, DecoderConfig, FrameDecoded, PixelFormat,
};
use reims_vgpu_video_core::encoder_backend::{
    BackendEncoder, EncodedFrame, EncoderBackend, EncoderCapabilities, EncoderConfig,
    EncoderSettings, SourceFrame,
};
use reims_vgpu_video_core::status::OsStatus;

/// A backend as the session reaches it: `inner`, logged (module doc).
pub(super) struct LoggedBackend<P: Platform> {
    pub(super) inner: Arc<dyn DecoderBackend>,
    pub(super) name: &'static str,
    pub(super) platform: Arc<P>,
}

impl<P: Platform> DecoderBackend for LoggedBackend<P> {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        self.inner.capabilities()
    }

    fn open(&self, config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        match self.inner.open(config) {
            Ok(inner) => {
                log::info!(
                    "{} decoder opened: {:?} {}x{}, {} bytes of configuration, output {:?}",
                    self.name,
                    config.codec,
                    config.width,
                    config.height,
                    config.configuration.len(),
                    inner.output_format()
                );
                Ok(Box::new(LoggedDecoder {
                    inner,
                    name: self.name,
                    platform: Arc::clone(&self.platform),
                    decoded: 0,
                    failed: 0,
                    emitted: 0,
                    busy: std::time::Duration::ZERO,
                }))
            }
            Err(error) => {
                log::warn!(
                    "{} decoder for {:?} {}x{} not opened: {error}",
                    self.name,
                    config.codec,
                    config.width,
                    config.height
                );
                Err(error)
            }
        }
    }
}

/// One decoder of a [`LoggedBackend`], counting what it decodes.
struct LoggedDecoder<P: Platform> {
    inner: Box<dyn BackendDecoder>,
    name: &'static str,
    platform: Arc<P>,
    decoded: usize,
    failed: usize,
    emitted: usize,
    busy: std::time::Duration,
}

impl<P: Platform> BackendDecoder for LoggedDecoder<P> {
    fn output_format(&self) -> PixelFormat {
        self.inner.output_format()
    }

    fn decode(
        &mut self,
        frame: &CompressedFrame<'_>,
    ) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        let clock = self.platform.clock();
        let start = clock.now_mono();
        let decoded = self.inner.decode(frame);
        let took = clock.now_mono().saturating_duration_since(start);
        self.busy += took;
        match &decoded {
            Ok(out) => {
                self.decoded += 1;
                self.emitted += out.len();
                log::debug!(
                    "{} decode: frame {} of {} bytes in {:.3} ms, {} out",
                    self.name,
                    frame.frame_id,
                    frame.data.len(),
                    took.as_secs_f64() * 1e3,
                    out.len()
                );
            }
            Err(error) => {
                self.failed += 1;
                log::warn!(
                    "{} decode: frame {} of {} bytes failed: {error}",
                    self.name,
                    frame.frame_id,
                    frame.data.len()
                );
            }
        }
        decoded
    }

    fn flush(&mut self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        let flushed = self.inner.flush();
        if let Ok(out) = &flushed {
            self.emitted += out.len();
            log::debug!("{} flush: {} out", self.name, out.len());
        }
        flushed
    }

    fn can_accept(&self, config: &DecoderConfig) -> bool {
        self.inner.can_accept(config)
    }

    /// The backend's own answer: without it the session copies one frame at a time, and the
    /// GPU waits through every copy back.
    fn decodes_while_copying(&self) -> bool {
        self.inner.decodes_while_copying()
    }

    /// The backend's own answer: without it the session lends an AV1 unit one buffer a frame
    /// output, where Apple's decoder lends one a frame decoded and holds it while it is
    /// referenced.
    fn frames_decoded(&mut self) -> Vec<FrameDecoded> {
        self.inner.frames_decoded()
    }
}

impl<P: Platform> Drop for LoggedDecoder<P> {
    fn drop(&mut self) {
        log::info!(
            "{} decoder closed: {} frames decoded, {} failed, {} out, {:.1} ms in the backend",
            self.name,
            self.decoded,
            self.failed,
            self.emitted,
            self.busy.as_secs_f64() * 1e3
        );
    }
}

/// An encoder backend as the session reaches it: `inner`, logged as a [`LoggedBackend`] is: each
/// encoder's opening and end at info, and each frame it encodes at debug, with the time the
/// backend took.
pub(super) struct LoggedEncoders<P: Platform> {
    pub(super) inner: Arc<dyn EncoderBackend>,
    pub(super) name: &'static str,
    pub(super) platform: Arc<P>,
}

impl<P: Platform> EncoderBackend for LoggedEncoders<P> {
    fn encoders(&self) -> Vec<EncoderCapabilities> {
        self.inner.encoders()
    }

    /// The backend's own answer, logged where it is a status: VideoToolbox's -12912 while every
    /// hardware encoder session is taken, which `+enc` answers as Apple's host does.
    fn session_capabilities(&self, codec: Codec) -> Result<Option<EncoderCapabilities>, OsStatus> {
        let answered = self.inner.session_capabilities(codec);
        if let Err(status) = answered {
            log::warn!("{} encoder for {codec:?} not made now: {status}", self.name);
        }
        answered
    }

    fn open(&self, config: &EncoderConfig) -> Result<Box<dyn BackendEncoder>, BackendError> {
        match self.inner.open(config) {
            Ok(inner) => {
                log::info!(
                    "{} encoder opened: {:?} {}x{}, {:?}",
                    self.name,
                    config.codec,
                    config.width,
                    config.height,
                    config.settings
                );
                Ok(Box::new(LoggedEncoder {
                    inner,
                    name: self.name,
                    platform: Arc::clone(&self.platform),
                    encoded: 0,
                    failed: 0,
                    out: 0,
                    bytes: 0,
                    busy: std::time::Duration::ZERO,
                }))
            }
            Err(error) => {
                log::warn!(
                    "{} encoder for {:?} {}x{} not opened: {error}",
                    self.name,
                    config.codec,
                    config.width,
                    config.height
                );
                Err(error)
            }
        }
    }

    fn check_property(
        &self,
        codec: Codec,
        key: &str,
        value: &reims_vgpu_video_core::plist::Value,
    ) -> Result<(), OsStatus> {
        let checked = self.inner.check_property(codec, key, value);
        if let Err(status) = checked {
            log::info!("{} encoder refuses {key} = {value:?}: {status}", self.name);
        }
        checked
    }
}

/// One encoder of a [`LoggedEncoders`], counting what it encodes.
struct LoggedEncoder<P: Platform> {
    inner: Box<dyn BackendEncoder>,
    name: &'static str,
    platform: Arc<P>,
    encoded: usize,
    failed: usize,
    out: usize,
    bytes: usize,
    busy: std::time::Duration,
}

impl<P: Platform> LoggedEncoder<P> {
    fn count(&mut self, frames: &[EncodedFrame]) {
        self.out += frames.len();
        self.bytes += frames.iter().map(|frame| frame.data.len()).sum::<usize>();
    }
}

impl<P: Platform> BackendEncoder for LoggedEncoder<P> {
    fn configuration(&self) -> Vec<u8> {
        self.inner.configuration()
    }

    fn encode(&mut self, frame: &SourceFrame<'_>) -> Result<Vec<EncodedFrame>, BackendError> {
        let clock = self.platform.clock();
        let start = clock.now_mono();
        let encoded = self.inner.encode(frame);
        let took = clock.now_mono().saturating_duration_since(start);
        self.busy += took;
        match &encoded {
            Ok(out) => {
                self.encoded += 1;
                self.count(out);
                log::debug!(
                    "{} encode: frame {} {:?} {}x{} in {:.3} ms, {} out of {} bytes",
                    self.name,
                    frame.frame_id,
                    frame.pixel_format,
                    frame.width,
                    frame.height,
                    took.as_secs_f64() * 1e3,
                    out.len(),
                    out.iter().map(|frame| frame.data.len()).sum::<usize>()
                );
            }
            Err(error) => {
                self.failed += 1;
                log::warn!(
                    "{} encode: frame {} failed: {error}",
                    self.name,
                    frame.frame_id
                );
            }
        }
        encoded
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>, BackendError> {
        let flushed = self.inner.flush();
        if let Ok(out) = &flushed {
            self.count(out);
            log::debug!("{} encoder flush: {} out", self.name, out.len());
        }
        flushed
    }

    fn reconfigure(&mut self, settings: &EncoderSettings) -> Result<(), BackendError> {
        let reconfigured = self.inner.reconfigure(settings);
        match &reconfigured {
            Ok(()) => log::info!("{} encoder reconfigured: {settings:?}", self.name),
            Err(error) => log::info!("{} encoder not reconfigured: {error}", self.name),
        }
        reconfigured
    }

    /// The guest's `epfr`, passed on: the VideoToolbox backend prepares its session there, as
    /// Apple's host prepares its encoder, and the default would leave it to the first frame.
    fn prepare(&mut self) -> Result<(), BackendError> {
        let prepared = self.inner.prepare();
        if let Err(error) = &prepared {
            log::info!("{} encoder not prepared: {error}", self.name);
        }
        prepared
    }
}

impl<P: Platform> Drop for LoggedEncoder<P> {
    fn drop(&mut self) {
        log::info!(
            "{} encoder closed: {} frames encoded, {} failed, {} out of {} bytes, {:.1} ms in the \
             backend",
            self.name,
            self.encoded,
            self.failed,
            self.out,
            self.bytes,
            self.busy.as_secs_f64() * 1e3
        );
    }
}
