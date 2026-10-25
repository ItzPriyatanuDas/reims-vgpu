//! What an encoder backend gives the host session: the encoders it offers,
//! and encoders that turn the guest's source frames into compressed
//! samples. The trait is the host's, whatever encodes: VA-API, NVENC, or
//! VideoToolbox on a macOS host, and a stand-in in the tests.
//!
//! The session does everything the guest sees: the encoder list (`encs`),
//! the properties, the pixel buffer attributes it asks the guest for
//! (`Epba`), the format description and the sample buffers it emits
//! (`Efds`, `Eemt`). A backend only encodes. It opens an encoder when the
//! guest starts its session (`esta`), encodes each frame the guest sends
//! (`efrm`), returns what it held back when the guest completes its frames
//! (`ecfr`), and is dropped when the guest invalidates the encoder or ends
//! the session.

use crate::backend::{BackendError, Codec, PixelFormat};
use crate::status::OsStatus;
use crate::surfaces::PlaneSource;
use crate::wire::message::MediaTime;

/// What a backend encodes of one codec: one entry of the host's encoder
/// list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderCapabilities {
    /// The codec.
    pub codec: Codec,
    /// The profiles it encodes, as the bitstream numbers them:
    /// `profile_idc` for H.264 (66 Baseline, 77 Main, 100 High),
    /// `general_profile_idc` for HEVC (1 Main, 2 Main 10).
    pub profiles: Vec<u32>,
    /// The widest picture it encodes.
    pub max_width: u32,
    /// The tallest picture it encodes.
    pub max_height: u32,
    /// The source pixel formats it takes, in the order the host asks the
    /// guest for them: the first is the one the guest's buffer pool makes.
    pub source_formats: Vec<PixelFormat>,
    /// Whether it can reorder frames (B-frames): an encoder that cannot
    /// emits every frame in presentation order.
    pub frame_reordering: bool,
}

/// How the guest's entropy coder setting (`H264EntropyMode`) asks H.264 to
/// be coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropyMode {
    /// `CAVLC`.
    Cavlc,
    /// `CABAC`.
    Cabac,
}

/// The colour a stream's VUI is to carry, as ITU-T H.273 code points, from
/// the guest's colour properties. `None` is unspecified.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColourDescription {
    /// `colour_primaries`.
    pub primaries: Option<u8>,
    /// `transfer_characteristics`.
    pub transfer: Option<u8>,
    /// `matrix_coefficients`.
    pub matrix: Option<u8>,
}

/// The settings the guest gave the encoder through its properties, twice:
/// mapped into fields a backend with encoders of its own design reads, each
/// `None`, or false, where the guest set nothing and the backend chooses;
/// and as the guest set them, in [`EncoderSettings::properties`], for a
/// backend whose encoders take VideoToolbox's properties themselves.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncoderSettings {
    /// The profile `ProfileLevel` names, as [`EncoderCapabilities::profiles`]
    /// numbers it.
    pub profile: Option<u32>,
    /// The level `ProfileLevel` names, as the bitstream numbers it
    /// (`level_idc`, 41 for 4.1; `general_level_idc`, 123 for 4.1); `None`
    /// for an automatic level.
    pub level: Option<u32>,
    /// `AverageBitRate`, in bits per second.
    pub average_bit_rate: Option<u32>,
    /// The tightest of `DataRateLimits`, in bits per second.
    pub max_bit_rate: Option<u32>,
    /// `Quality`, from 0 (smallest) to 1 (best).
    pub quality: Option<f32>,
    /// `MaxKeyFrameInterval`, in frames.
    pub max_key_frame_interval: Option<u32>,
    /// `MaxKeyFrameIntervalDuration`, in seconds.
    pub max_key_frame_interval_duration: Option<f64>,
    /// `ExpectedFrameRate`, in frames per second.
    pub expected_frame_rate: Option<f64>,
    /// `AllowFrameReordering`: whether B-frames may be used.
    pub allow_frame_reordering: bool,
    /// `RealTime`: whether the guest encodes live.
    pub real_time: bool,
    /// `H264EntropyMode`.
    pub entropy: Option<EntropyMode>,
    /// `ColorPrimaries`, `TransferFunction` and `YCbCrMatrix`.
    pub colour: ColourDescription,
    /// Every property the guest set and the session took, by its
    /// `kVTCompressionPropertyKey_*` string, with its value as the guest
    /// sent it, sorted by key: what a VideoToolbox backend sets on its own
    /// session as it is.
    pub properties: Vec<(String, plist::Value)>,
}

/// What an encoder is opened with, when the guest starts its session.
#[derive(Clone, Debug, PartialEq)]
pub struct EncoderConfig {
    /// The codec.
    pub codec: Codec,
    /// The picture's width, from `esta`.
    pub width: u32,
    /// The picture's height.
    pub height: u32,
    /// The settings so far.
    pub settings: EncoderSettings,
}

/// One source frame, read from the guest's surface. Its planes are valid
/// only for the call they are passed to: a backend copies what it needs.
#[derive(Debug)]
pub struct SourceFrame<'a> {
    /// The guest's identifier for the frame, which each sample encoded
    /// from it carries back.
    pub frame_id: i64,
    /// The presentation time stamp.
    pub presentation_time: MediaTime,
    /// The duration; invalid when the guest gave none.
    pub duration: MediaTime,
    /// Whether the guest forced a key frame (`kVTEncodeFrameOptionKey_ForceKeyFrame`).
    pub force_key_frame: bool,
    /// The pixel format, one of the capabilities' source formats.
    pub pixel_format: PixelFormat,
    /// The width in pixels.
    pub width: u32,
    /// The height in pixels.
    pub height: u32,
    /// The planes: luma, then interleaved chroma, each with at least the
    /// picture's rows and each row's bytes.
    pub planes: &'a [PlaneSource<'a>],
}

/// One encoded frame: a sample, as the guest's format description says
/// samples are; or a frame the encoder dropped or failed, with none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    /// The identifier of the source frame it was encoded from.
    pub frame_id: i64,
    /// The sample: NAL units, each after a 4-byte big-endian length, with
    /// no parameter sets, which travel in the configuration record. Empty
    /// for a frame the encoder dropped (`kVTEncodeInfo_FrameDropped`): the
    /// guest is told it was dropped, with no sample. Empty too for a frame
    /// with an [`EncodedFrame::error`].
    pub data: Vec<u8>,
    /// The presentation time stamp: the source frame's.
    pub presentation_time: MediaTime,
    /// The decode time stamp; invalid when it equals the presentation time
    /// stamp, as with no reordering.
    pub decode_time: MediaTime,
    /// The duration: the source frame's.
    pub duration: MediaTime,
    /// Whether a decoder can start here: an IDR picture for H.264, an IRAP
    /// one for HEVC.
    pub sync: bool,
    /// Whether it references other frames.
    pub depends_on_others: bool,
    /// Whether other frames reference it.
    pub depended_on_by_others: bool,
    /// Why the encoder failed this frame after it took it, or `None`. A
    /// failed frame has no sample: the guest gets its `Eemt` with the
    /// error's status, in its place among the others, as Apple's host
    /// forwards each of its encoder's emits with its own status. So an
    /// error stands for its own frame alone, and the frames emitted with it
    /// keep their samples.
    pub error: Option<BackendError>,
}

impl EncodedFrame {
    /// A frame the encoder dropped: no data, no times and no error.
    pub fn dropped(frame_id: i64) -> Self {
        Self {
            frame_id,
            data: Vec::new(),
            presentation_time: MediaTime::INVALID,
            decode_time: MediaTime::INVALID,
            duration: MediaTime::INVALID,
            sync: false,
            depends_on_others: false,
            depended_on_by_others: false,
            error: None,
        }
    }

    /// A frame the encoder took and failed with `error`, which has no
    /// sample.
    pub fn failed(frame_id: i64, error: BackendError) -> Self {
        Self {
            error: Some(error),
            ..Self::dropped(frame_id)
        }
    }
}

/// A backend: the encoders it offers, and the means to open them.
///
/// One backend serves every encoder session of every host session, from
/// several threads at once. `encoders` and `session_capabilities` are
/// asked on the device's thread as the guest lists encoders (`encs`) and
/// opens a session (`+enc`), so they answer with no driver call that may
/// stall: the guest's queue waits for them. `open` and `check_property` run
/// on the session's own thread. None is called inside a lend of a guest
/// surface.
pub trait EncoderBackend: Send + Sync {
    /// The encoders the host lists to the guest (`encs`). A codec that is
    /// not here is one the guest is never offered.
    fn encoders(&self) -> Vec<EncoderCapabilities>;

    /// What a new session of `codec` gets, asked at each `+enc` for a
    /// listed codec: the capabilities its session is made with, None where
    /// the backend does not list the codec, or the status `+enc` answers
    /// where it lists the codec but cannot make an encoder of it now.
    /// Apple's host lists its encoders whatever their state and answers
    /// `+enc` with its encoder's creation status: -12912 from AVE while
    /// every hardware encoder session of the Mac is taken. The default is
    /// the listed entry.
    fn session_capabilities(&self, codec: Codec) -> Result<Option<EncoderCapabilities>, OsStatus> {
        Ok(self
            .encoders()
            .into_iter()
            .find(|listed| listed.codec == codec))
    }

    /// Opens an encoder for `config`, at the guest's `esta`, which gets an
    /// `Err` as its status.
    fn open(&self, config: &EncoderConfig) -> Result<Box<dyn BackendEncoder>, BackendError>;

    /// Whether this backend's encoders of `codec` take `key` set to
    /// `value`, asked each time the guest sets a property the session
    /// offers, before its session starts or after. `Err` is the status the
    /// guest gets for it, and the property keeps its old value: -12900
    /// (`kVTPropertyNotSupportedErr`) for a value the backend cannot honour,
    /// say, as VideoToolbox's encoders answer. The default takes all.
    fn check_property(
        &self,
        codec: Codec,
        key: &str,
        value: &plist::Value,
    ) -> Result<(), OsStatus> {
        let _ = (codec, key, value);
        Ok(())
    }
}

/// One open encoder, which its session calls on the session's thread, one
/// call at a time, with no guest surface lent: a source frame's samples are
/// a copy the session made.
pub trait BackendEncoder: Send {
    /// The decoder configuration record of the stream: `avcC` for H.264 (an
    /// AVCDecoderConfigurationRecord, with the SPS and PPS and 4-byte NAL
    /// unit lengths), `hvcC` for HEVC. The session asks before each
    /// emission and makes a new format description when it changes.
    fn configuration(&self) -> Vec<u8>;

    /// Encodes one frame, returning the frames ready for output, in decode
    /// order: this one, earlier ones it held back, or none yet.
    ///
    /// Each frame the encoder takes comes back once, in this call or a
    /// later one: as a sample, dropped, or with its own
    /// [`EncodedFrame::error`], so that one frame's failure loses none of
    /// the others. `Err` is for a frame the encoder did not take: the
    /// guest's `efrm` is answered with its status, and the frame is never
    /// emitted. Frames the encoder took and never gives back are answered
    /// at the guest's next `ecfr` with -12912 and no sample.
    fn encode(&mut self, frame: &SourceFrame<'_>) -> Result<Vec<EncodedFrame>, BackendError>;

    /// Returns every frame still held back, in decode order, each with its
    /// own result, as [`BackendEncoder::encode`] does.
    fn flush(&mut self) -> Result<Vec<EncodedFrame>, BackendError>;

    /// Takes new settings, which the guest set after its session started:
    /// a bit rate, say. A setting the encoder cannot change while it runs
    /// is an [`BackendError::Unsupported`], which the guest gets as -12900
    /// (`kVTPropertyNotSupportedErr`).
    fn reconfigure(&mut self, settings: &EncoderSettings) -> Result<(), BackendError>;

    /// Prepares to encode, at the guest's `epfr`: Apple's host calls its
    /// encoder's `PrepareToEncodeFrames` there, and only there, and answers
    /// with its status. The guest's VideoToolbox sends `epfr` once, before
    /// the first `efrm`: when its client prepares the session, or, when the
    /// client never does, inside the client's first encode, after every
    /// property set before it. An encoder that takes some settings only
    /// before it is prepared, as AVE takes `DataRateLimits`, prepares here,
    /// so that a setting made after it is lost as behind Apple's host.
    /// Called again, it does nothing. The default has nothing to prepare.
    fn prepare(&mut self) -> Result<(), BackendError> {
        Ok(())
    }
}

/// The backend of a host with no encoders: it lists none, and so the guest
/// opens none.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEncoders;

impl EncoderBackend for NoEncoders {
    fn encoders(&self) -> Vec<EncoderCapabilities> {
        Vec::new()
    }

    fn open(&self, config: &EncoderConfig) -> Result<Box<dyn BackendEncoder>, BackendError> {
        Err(BackendError::Unsupported(format!(
            "no {:?} encoder",
            config.codec
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_encoders_lists_none_and_opens_none() {
        assert!(NoEncoders.encoders().is_empty());
        let config = EncoderConfig {
            codec: Codec::H264,
            width: 64,
            height: 48,
            settings: EncoderSettings::default(),
        };
        assert!(matches!(
            NoEncoders.open(&config),
            Err(BackendError::Unsupported(_))
        ));
    }
}
