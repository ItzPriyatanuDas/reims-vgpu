//! What a backend gives the devices.
//!
//! - [`Backend`] is the whole of one, as the host session and the glue
//!   take it: its decoders, its encoders ([`crate::encoder_backend`]), its
//!   motion estimation ([`crate::motion`]) and its JPEG decoder
//!   ([`crate::jpeg`]), each optional.
//! - [`DecoderBackend`] is its decoders: the codecs it decodes, and
//!   decoders that turn compressed samples into frames the session copies
//!   into the guest's surfaces.
//!
//! The session asks for everything the guest sees; a backend only decodes.
//! It opens a decoder when the guest starts a decode session (`dsta`),
//! decodes each frame the guest sends (`dfrm`), flushes when the guest
//! finishes delayed frames (`dfin`), and is dropped when the guest
//! invalidates the decoder or ends the session.

use std::fmt;
use std::sync::Arc;

use crate::device_memory::GuestTarget;
use crate::encoder_backend::{EncoderBackend, NoEncoders};
use crate::fourcc::FourCc;
use crate::jpeg::JpegDecoder;
use crate::motion::MotionEstimationBackend;
use crate::status::{OsStatus, Status};
use crate::surfaces::PlaneDestination;
use crate::wire::message::MediaTime;

/// A codec the host can offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264, `avc1`.
    H264,
    /// HEVC, `hvc1`.
    Hevc,
    /// AV1, `av01`.
    Av1,
    /// VP9, `vp09`.
    Vp9,
}

impl Codec {
    /// The CoreMedia codec type the guest names it by.
    pub const fn fourcc(self) -> FourCc {
        FourCc::new(match self {
            Self::H264 => *b"avc1",
            Self::Hevc => *b"hvc1",
            Self::Av1 => *b"av01",
            Self::Vp9 => *b"vp09",
        })
    }

    /// The codec a CoreMedia codec type names, if this side knows it.
    pub fn from_fourcc(codec: FourCc) -> Option<Self> {
        [Self::H264, Self::Hevc, Self::Av1, Self::Vp9]
            .into_iter()
            .find(|known| known.fourcc() == codec)
    }
}

/// A pixel format the host decodes into: the CoreVideo formats a guest
/// pixel buffer can have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit 4:2:0, Y then interleaved CbCr, video range: `420v`.
    Nv12VideoRange,
    /// The same, full range: `420f`.
    Nv12FullRange,
    /// 10-bit 4:2:0 in 16-bit samples, video range: `x420`.
    P010VideoRange,
    /// The same, full range: `xf20`.
    P010FullRange,
}

impl PixelFormat {
    /// The CoreVideo pixel format type.
    pub const fn fourcc(self) -> FourCc {
        FourCc::new(match self {
            Self::Nv12VideoRange => *b"420v",
            Self::Nv12FullRange => *b"420f",
            Self::P010VideoRange => *b"x420",
            Self::P010FullRange => *b"xf20",
        })
    }

    /// The format a CoreVideo pixel format type names, if it is one of
    /// these.
    pub fn from_fourcc(format: FourCc) -> Option<Self> {
        [
            Self::Nv12VideoRange,
            Self::Nv12FullRange,
            Self::P010VideoRange,
            Self::P010FullRange,
        ]
        .into_iter()
        .find(|known| known.fourcc() == format)
    }

    /// The bytes one sample takes: 1 for 8-bit formats, 2 for 10-bit ones.
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Nv12VideoRange | Self::Nv12FullRange => 1,
            Self::P010VideoRange | Self::P010FullRange => 2,
        }
    }
}

/// One profile a backend decodes, and the highest level it decodes it at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileSupport {
    /// The profile as the bitstream numbers it: `profile_idc` for H.264
    /// (66 Baseline, 77 Main, 100 High, ...), `general_profile_idc` for
    /// HEVC (1 Main, 2 Main 10), `seq_profile` for AV1 (0 Main, which is 8
    /// and 10 bits at 4:2:0 and monochrome), the profile for VP9 (0 is 8
    /// bits at 4:2:0, 2 is 10 and 12 bits at 4:2:0).
    pub profile: u32,
    /// The highest level, as the bitstream numbers it: `level_idc` for
    /// H.264 (51 is 5.1), `general_level_idc` for HEVC (153 is 5.1),
    /// `seq_level_idx` for AV1 (13 is 5.1), `vpcC`'s level for VP9 (51 is
    /// 5.1).
    /// `None` when the backend knows no limit, as VA-API reports none: the
    /// host then lists what Apple's decoders list.
    pub max_level: Option<u32>,
}

/// What a backend decodes of one codec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecCapabilities {
    /// The codec.
    pub codec: Codec,
    /// The profiles it decodes.
    pub profiles: Vec<ProfileSupport>,
    /// The widest picture it decodes.
    pub max_width: u32,
    /// The tallest picture it decodes.
    pub max_height: u32,
}

/// What a decoder is opened with, from the guest's format description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecoderConfig {
    /// The codec.
    pub codec: Codec,
    /// The picture's width.
    pub width: u32,
    /// The picture's height.
    pub height: u32,
    /// The decoder configuration record: `avcC` for H.264 (an
    /// AVCDecoderConfigurationRecord, with the SPS and PPS), `hvcC` for
    /// HEVC, each with samples in its NAL unit length format; `av1C` for
    /// AV1 (an AV1CodecConfigurationRecord, usually with the sequence
    /// header OBU), with samples that are temporal units in the low
    /// overhead format; `vpcC` for VP9 (the box's version and flags, then
    /// a VPCodecConfigurationRecord), with samples that are frames or
    /// superframes.
    pub configuration: Vec<u8>,
}

/// One compressed frame to decode: a sample of length-prefixed NAL units.
#[derive(Clone, Copy, Debug)]
pub struct CompressedFrame<'a> {
    /// The guest's identifier for the frame. Each frame decoded from this
    /// sample carries it back.
    pub frame_id: i64,
    /// The sample's bytes.
    pub data: &'a [u8],
    /// The sizes of the samples in `data` when it holds several; one entry
    /// or none for one sample.
    pub sample_sizes: &'a [u32],
    /// The presentation time stamp.
    pub presentation_time: MediaTime,
    /// The decode time stamp.
    pub decode_time: MediaTime,
    /// The duration.
    pub duration: MediaTime,
    /// A new configuration when the guest's format description changed
    /// since the last frame (a new `avcC`, a new size); the decoder applies
    /// it before this frame.
    pub new_config: Option<&'a DecoderConfig>,
}

/// A backend: the codecs it offers, and decoders for them.
///
/// One backend serves every decoder session of every host session, from
/// several threads at once. `capabilities` is asked on the device's thread
/// as the guest lists codecs (`decs`, `dcap`) and opens a session
/// (`+dec`), so it answers from what the backend found when it was made,
/// with no driver call that may stall: the guest's queue waits for it.
/// `open` runs on the session's own thread, at its `dsta`. Neither is
/// called inside a lend of a guest surface.
pub trait DecoderBackend: Send + Sync {
    /// The codecs the host lists to the guest (`decs`, `dcap`). A codec
    /// that is not here is one the guest is never offered.
    fn capabilities(&self) -> Vec<CodecCapabilities>;

    /// Opens a decoder for `config`. An `Err` is the status the guest's
    /// `dsta` gets ([`BackendError::status`]), and the session decodes
    /// nothing.
    fn open(&self, config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError>;
}

/// One frame an AV1 or VP9 decode took, as the backend's parser saw it: a
/// frame header decoded, or a frame shown again (`show_existing_frame`), of
/// any spatial layer, or for VP9 a frame refused. Apple's AV1 and VP9
/// decoders lend a guest buffer for each, and hold it as long as the frame
/// is referenced, so the session does too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameDecoded {
    /// The frame's number: the same for a frame shown again as for its
    /// decode, and never another frame's while the session lasts.
    pub picture: u64,
    /// Whether the frame was shown again rather than decoded here.
    pub shown_again: bool,
    /// Whether the frame was refused rather than decoded: a VP9 frame
    /// with an error, one after it in its sample, or one after a frame
    /// that failed. Apple's VP9 decoder lends it a buffer, writes nothing
    /// into it, and lets it go at the next frame; no slot holds a frame
    /// after it ([`BackendDecoder::frames_decoded`]).
    pub refused: bool,
    /// Its width (AV1's `UpscaledWidth`, VP9's `FrameWidth`).
    pub width: u32,
    /// Its height (`FrameHeight`).
    pub height: u32,
    /// The frame each of the codec's eight reference slots holds after it,
    /// by number.
    pub references: [Option<u64>; 8],
    /// The frame it loads its saved state from (`primary_ref_frame`), by
    /// number, where it loads one.
    pub primary_reference: Option<u64>,
    /// Whether the decode outputs it: the frames that do are those
    /// [`BackendDecoder::decode`] returns, in the same order.
    pub output: bool,
    /// The largest frame its sequence codes (`max_frame_width_minus_1 + 1`
    /// by `max_frame_height_minus_1 + 1`); for VP9, which codes none, the
    /// frame's own size.
    pub largest: (u32, u32),
}

/// One open decoder, which its session calls on the session's thread, one
/// call at a time. No call is made inside a lend of a guest surface.
pub trait BackendDecoder: Send {
    /// The pixel format it decodes into, which the session asks the guest
    /// to allocate (`Dpba`) before the first frame. It follows from the
    /// configuration: bit depth and range.
    fn output_format(&self) -> PixelFormat;

    /// Decodes one frame, returning the frames ready for output, in the
    /// order they are to be emitted: this one, earlier ones it held back,
    /// or none yet.
    fn decode(
        &mut self,
        frame: &CompressedFrame<'_>,
    ) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError>;

    /// Returns every frame still held back, in emission order.
    fn flush(&mut self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError>;

    /// Whether this decoder can go on with `config` without being
    /// reopened.
    fn can_accept(&self, config: &DecoderConfig) -> bool;

    /// Whether the session may decode the next sample while one of this
    /// decoder's frames is copied out on another thread. Yes suits a
    /// decoder whose `decode` hands the work to a device and returns: the
    /// next frame then decodes on the device while the last one is copied
    /// back. A decoder that says yes lets `decode`, a reconfiguration's
    /// included, run on the session's thread while a frame of its own
    /// whose [`DecodedFrame::wait`] has returned is read back and copied
    /// on another thread. No by default.
    fn decodes_while_copying(&self) -> bool {
        false
    }

    /// Every frame the last [`BackendDecoder::decode`] decoded or showed
    /// again, in order, where the codec's frames are not one a sample: an
    /// AV1 temporal unit's or a VP9 superframe's hidden frames, frames
    /// shown again and spatial layers. The session asks right after each
    /// decode, and lends and holds the guest's buffers by it as Apple's AV1
    /// and VP9 decoders do. After a VP9 decode that failed it is the
    /// frames the sample carried, those before the failure decoded and
    /// output none, then the one that failed and each after it refused
    /// ([`FrameDecoded::refused`]): Apple's VP9 decoder lends a buffer for
    /// each (measured on Apple's host session on the M3 Max, macOS 27.0.1,
    /// 2026-10-25). After any other decode that failed the session reads
    /// none. Each decode's are read once: a decode that fails before the
    /// codec's decoder takes the sample says none. None, the default,
    /// leaves the session one buffer for each frame output.
    fn frames_decoded(&mut self) -> Vec<FrameDecoded> {
        Vec::new()
    }
}

/// A decoded frame, waiting to be copied into a guest surface.
///
/// The session's thread calls it, or, where the decoder decodes while
/// copying ([`BackendDecoder::decodes_while_copying`]), a copy thread of
/// the session's while the decoder goes on: one call at a time either way.
///
/// The session copies it in two steps: [`DecodedFrame::read_back`], which
/// makes every call into the backend's driver the copy needs, then the
/// copy itself on the CPU ([`FrameReadBack::copy_to`]), the only step that
/// runs while the guest's surface is lent. A host may lend one surface at
/// a time in the process, as reims-vgpu's mapper does, so a driver call
/// that never returned inside a lend would hold every other lend for ever.
pub trait DecodedFrame: Send {
    /// The identifier of the compressed frame it was decoded from.
    fn frame_id(&self) -> i64;

    /// The pixel format of its samples.
    fn pixel_format(&self) -> PixelFormat;

    /// Its width in pixels.
    fn width(&self) -> u32;

    /// Its height in pixels.
    fn height(&self) -> u32;

    /// Reads the frame back where the CPU reaches it, and returns what
    /// copies it from there: every call into the backend's driver that a
    /// copy needs is made here, the wait for the decode among them, and
    /// none in the copy. What it returns holds the frame there until it is
    /// dropped, after the copy. The session calls it once for each copy,
    /// after [`DecodedFrame::wait`] where it calls that.
    fn read_back(&self) -> Result<Box<dyn FrameReadBack + '_>, BackendError>;

    /// Waits until the frame's picture is whole, for a backend whose
    /// decodes finish on a device after `decode` returns. The session calls
    /// it before `read_back` where the decoder decodes while copying
    /// ([`BackendDecoder::decodes_while_copying`]), and decodes the next
    /// sample only once it has returned. The default waits for nothing.
    fn wait(&self) -> Result<(), BackendError> {
        Ok(())
    }

    /// Writes the frame into the guest's buffer with the backend's own
    /// device, so that the CPU copies nothing: its picture, or as much of
    /// it as fits, at the buffer's top left
    /// ([`GuestTarget::written_size`]), within the planes `target` checked
    /// and nowhere else. The session calls it in place of `read_back` and
    /// the copy, where the host lends the buffer's memory to devices and
    /// zero copy is on, outside any lend, while it holds the buffer, after
    /// [`DecodedFrame::wait`] where it calls that. Every driver call of the
    /// write is made before it returns, and the frame is in the guest's
    /// pages when it returns [`DeviceWrite::Written`].
    ///
    /// [`DeviceWrite::Declined`] says why the device does not write this
    /// buffer, as where its driver refuses the buffer's layout: the session
    /// copies the frame with the CPU instead, and logs why once a session.
    /// An `Err` is a frame that could not be read back either, which fails
    /// as a read back's failure does. The default declines.
    fn write_to_guest(&self, target: &GuestTarget<'_>) -> Result<DeviceWrite, BackendError> {
        let _ = target;
        Ok(DeviceWrite::Declined(
            "this backend writes no guest buffer itself".to_owned(),
        ))
    }
}

/// What [`DecodedFrame::write_to_guest`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceWrite {
    /// The frame is in the guest's buffer.
    Written,
    /// Nothing was written, for the reason given: the frame is copied by
    /// the CPU instead.
    Declined(String),
}

/// A decoded frame read back where the CPU reaches it
/// ([`DecodedFrame::read_back`]), and kept there until dropped.
pub trait FrameReadBack {
    /// Copies the frame into `planes`: the luma plane, then the chroma
    /// plane, each with its own bytes per row. A destination with fewer
    /// rows or narrower rows than the frame takes what fits. The CPU's
    /// copy alone, with no call into the backend's driver: the session
    /// runs it while the guest's surface is lent.
    fn copy_to(&self, planes: &mut [PlaneDestination<'_>]) -> Result<(), BackendError>;
}

/// The read back of a frame whose samples are already where the CPU
/// reaches them: `copy`, run as its [`FrameReadBack::copy_to`].
pub fn read_back_by<'a>(
    copy: impl Fn(&mut [PlaneDestination<'_>]) -> Result<(), BackendError> + 'a,
) -> Box<dyn FrameReadBack + 'a> {
    struct By<F>(F);
    impl<F: Fn(&mut [PlaneDestination<'_>]) -> Result<(), BackendError>> FrameReadBack for By<F> {
        fn copy_to(&self, planes: &mut [PlaneDestination<'_>]) -> Result<(), BackendError> {
            (self.0)(planes)
        }
    }
    Box::new(By(copy))
}

/// Why a backend could not do what it was asked. Each kind reaches the
/// guest as a status of its own ([`BackendError::status`] for a decoder),
/// and [`BackendError::Native`] as the backend API's own status.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// The codec, profile, level or size is not one the backend decodes.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The data does not decode.
    #[error("bad data: {0}")]
    BadData(String),
    /// The backend or its driver failed.
    #[error("malfunction: {0}")]
    Malfunction(String),
    /// A leading picture skipped, since it predicts from pictures before a
    /// random access point the decoding started at: an HEVC RASL picture
    /// of an IRAP picture with `NoRaslOutputFlag` 1. Apple's HEVC decoder
    /// answers it with `kVTVideoDecoderReferenceMissingErr` and
    /// `kVTDecodeInfo_SkippedLeadingFrameDropped`. No failure: the frames
    /// after it decode.
    #[error("skipped: {0}")]
    SkippedLeadingPicture(String),
    /// The backend's API failed with a status of its own that the guest's
    /// VideoToolbox knows, which reaches the guest as it is: VideoToolbox's
    /// on a macOS host. Apple's host sends its codec's own status so, for
    /// `esta` and for `efrm`, so a backend on VideoToolbox answers as
    /// Apple's does. A backend whose API's statuses a guest does not know,
    /// as VA-API's and NVIDIA's, uses the three kinds above. A status that
    /// is not an error (0 or more) is taken as a malfunction.
    #[error("status {status}: {message}")]
    Native {
        /// The API's status.
        status: OsStatus,
        /// What failed, for the log.
        message: String,
    },
}

impl BackendError {
    /// The status the guest sees for it from a decoder, as a VideoToolbox
    /// decoder would report it: the kind's, or a [`Self::Native`] status
    /// as it is.
    pub fn status(&self) -> OsStatus {
        match self {
            Self::Unsupported(_) => Status::VideoDecoderUnsupportedDataFormat.code(),
            Self::BadData(_) => Status::VideoDecoderBadData.code(),
            Self::Malfunction(_) => Status::VideoDecoderMalfunction.code(),
            Self::SkippedLeadingPicture(_) => Status::VideoDecoderReferenceMissing.code(),
            Self::Native { status, .. } => {
                Self::native_status(*status).unwrap_or(Status::VideoDecoderMalfunction.code())
            }
        }
    }

    /// The API status a [`Self::Native`] error carries, where it is an
    /// error.
    pub fn native_status(status: OsStatus) -> Option<OsStatus> {
        (status < 0).then_some(status)
    }
}

/// Copies 4:2:0 biplanar pixels from source planes into destination
/// planes, row by row: the luma plane's `height` rows of `width` samples,
/// and the chroma plane's half as many (rounded up) of `width` rounded up
/// to even, a Cb and Cr pair for every two luma samples, the last one too
/// at an odd width. A destination row or plane shorter than the source
/// takes what fits, and a row past either's addresses ends the plane. A
/// helper for [`FrameReadBack::copy_to`].
pub fn copy_biplanar(
    format: PixelFormat,
    width: u32,
    height: u32,
    sources: [(&[u8], usize); 2],
    destinations: &mut [PlaneDestination<'_>],
) {
    let width = usize::try_from(width).unwrap_or(0);
    let sample = format.bytes_per_sample();
    let row_bytes = [
        width.saturating_mul(sample),
        width.div_ceil(2).saturating_mul(2 * sample),
    ];
    let heights = [height, height.div_ceil(2)];
    for ((source, source_stride), (destination, (plane_height, row_bytes))) in
        sources.into_iter().zip(
            destinations
                .iter_mut()
                .zip(heights.into_iter().zip(row_bytes)),
        )
    {
        let rows = plane_height.min(destination.rows);
        for row in 0..usize::try_from(rows).unwrap_or(0) {
            let from = row
                .checked_mul(source_stride)
                .and_then(|start| source.get(start..));
            let to = row
                .checked_mul(destination.bytes_per_row)
                .and_then(|start| destination.bytes.get_mut(start..));
            let (Some(from), Some(to)) = (from, to) else {
                break;
            };
            let length = row_bytes
                .min(from.len())
                .min(to.len())
                .min(destination.bytes_per_row);
            if let (Some(to), Some(from)) = (to.get_mut(..length), from.get(..length)) {
                to.copy_from_slice(from);
            }
        }
    }
}

/// The decoders of a host with none: it lists no codec, and so the guest
/// opens no decoder and decodes in software.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDecoders;

impl DecoderBackend for NoDecoders {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        Vec::new()
    }

    fn open(&self, config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        Err(BackendError::Unsupported(format!(
            "no {:?} decoder",
            config.codec
        )))
    }
}

/// A backend as the video device takes it: what one host decodes and
/// encodes with, through one driver (VA-API, NVDEC and NVENC, or
/// VideoToolbox). Each part is optional, and a part that is `None` is one
/// the guest is never offered: the codec lists leave it out. What the guest
/// sees of each part is core's, the same on every backend.
#[derive(Clone, Default)]
pub struct Backend {
    /// What the host's log calls it, such as `VA-API`.
    pub name: String,
    /// Its decoders: `decs`, `dcap` and the decoder sessions.
    pub decoders: Option<Arc<dyn DecoderBackend>>,
    /// Its encoders: `encs` and the encoder sessions.
    pub encoders: Option<Arc<dyn EncoderBackend>>,
    /// Its motion estimation: `meps` and the motion-estimation sessions.
    /// Without it, `meps` lists no processor.
    pub motion: Option<Arc<dyn MotionEstimationBackend>>,
    /// Its JPEG decoder: the JPEG engine's `jcap` and `jdec`. Without one,
    /// the guest is told the host has no JPEG engine.
    pub jpeg: Option<Arc<dyn JpegDecoder>>,
}

impl Backend {
    /// A backend named `name` with no part: the guest is offered nothing,
    /// and does all its work in software.
    pub fn none(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Its decoders, or [`NoDecoders`].
    pub fn decoders_or_none(&self) -> Arc<dyn DecoderBackend> {
        self.decoders
            .clone()
            .unwrap_or_else(|| Arc::new(NoDecoders))
    }

    /// Its encoders, or [`NoEncoders`].
    pub fn encoders_or_none(&self) -> Arc<dyn EncoderBackend> {
        self.encoders
            .clone()
            .unwrap_or_else(|| Arc::new(NoEncoders))
    }

    /// What it offers, in one line for the host's log: each codec it
    /// decodes and encodes, with its profiles and largest size, whether it
    /// decodes JPEG, and, when it does, up to what size
    /// it estimates motion. It asks the parts for their lists, as `decs`
    /// and `encs` do.
    pub fn summary(&self) -> String {
        let decodes: Vec<String> = (self.decoders.iter())
            .flat_map(|d| d.capabilities())
            .map(|c| {
                let profiles: Vec<u32> = c.profiles.iter().map(|p| p.profile).collect();
                format!(
                    "{:?} {profiles:?} up to {}x{}",
                    c.codec, c.max_width, c.max_height
                )
            })
            .collect();
        let encodes: Vec<String> = (self.encoders.iter())
            .flat_map(|e| e.encoders())
            .map(|c| {
                format!(
                    "{:?} {:?} up to {}x{}",
                    c.codec, c.profiles, c.max_width, c.max_height
                )
            })
            .collect();
        let list = |items: Vec<String>| {
            if items.is_empty() {
                "nothing".to_owned()
            } else {
                items.join(", ")
            }
        };
        let motion = (self.motion.iter())
            .map(|motion| {
                let c = motion.capabilities();
                format!("; estimates motion up to {}x{}", c.max_width, c.max_height)
            })
            .collect::<String>();
        format!(
            "{}: decodes {}; encodes {}; {}{motion}",
            self.name,
            list(decodes),
            list(encodes),
            if self.jpeg.is_some() {
                "decodes JPEG"
            } else {
                "no JPEG"
            }
        )
    }
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend")
            .field("name", &self.name)
            .field("decoders", &self.decoders.is_some())
            .field("encoders", &self.encoders.is_some())
            .field("motion", &self.motion.is_some())
            .field("jpeg", &self.jpeg.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_and_formats_are_their_fourccs() {
        assert_eq!(Codec::from_fourcc(FourCc::new(*b"avc1")), Some(Codec::H264));
        assert_eq!(Codec::from_fourcc(FourCc::new(*b"av01")), Some(Codec::Av1));
        assert_eq!(Codec::from_fourcc(FourCc::new(*b"vp09")), Some(Codec::Vp9));
        assert_eq!(Codec::from_fourcc(FourCc::new(*b"mp4v")), None);
        assert_eq!(PixelFormat::Nv12VideoRange.fourcc().0, 875_704_438);
        assert_eq!(
            PixelFormat::from_fourcc(FourCc::new(*b"xf20")),
            Some(PixelFormat::P010FullRange)
        );
    }

    #[test]
    fn a_backend_with_no_part_offers_nothing() {
        let backend = Backend::none("nothing");
        assert!(backend.decoders_or_none().capabilities().is_empty());
        assert!(backend.encoders_or_none().encoders().is_empty());
        assert_eq!(
            backend.summary(),
            "nothing: decodes nothing; encodes nothing; no JPEG"
        );
        let config = DecoderConfig {
            codec: Codec::H264,
            width: 64,
            height: 48,
            configuration: Vec::new(),
        };
        assert!(matches!(
            NoDecoders.open(&config),
            Err(BackendError::Unsupported(_))
        ));
    }

    #[test]
    fn a_summary_names_each_codec_with_its_profiles_and_size() {
        struct H264Only;
        impl DecoderBackend for H264Only {
            fn capabilities(&self) -> Vec<CodecCapabilities> {
                vec![CodecCapabilities {
                    codec: Codec::H264,
                    profiles: vec![
                        ProfileSupport {
                            profile: 66,
                            max_level: None,
                        },
                        ProfileSupport {
                            profile: 100,
                            max_level: Some(51),
                        },
                    ],
                    max_width: 4096,
                    max_height: 2304,
                }]
            }
            fn open(
                &self,
                config: &DecoderConfig,
            ) -> Result<Box<dyn BackendDecoder>, BackendError> {
                NoDecoders.open(config)
            }
        }
        let backend = Backend {
            decoders: Some(Arc::new(H264Only)),
            ..Backend::none("test")
        };
        assert_eq!(
            backend.summary(),
            "test: decodes H264 [66, 100] up to 4096x2304; encodes nothing; no JPEG"
        );
        assert_eq!(
            format!("{backend:?}"),
            r#"Backend { name: "test", decoders: true, encoders: false, motion: false, jpeg: false }"#
        );
    }

    #[test]
    fn a_native_error_reaches_the_guest_as_its_api_answered() {
        let native = |status| BackendError::Native {
            status,
            message: "decode".into(),
        };
        assert_eq!(native(-12903).status(), -12903);
        assert_eq!(native(-17694).status(), -17694);
        assert_eq!(native(0).status(), Status::VideoDecoderMalfunction.code());
        assert_eq!(native(1).status(), Status::VideoDecoderMalfunction.code());
        assert_eq!(
            BackendError::BadData("x".into()).status(),
            Status::VideoDecoderBadData.code()
        );
        assert_eq!(
            BackendError::SkippedLeadingPicture("x".into()).status(),
            -17694
        );
    }

    #[test]
    fn biplanar_rows_copy_at_each_planes_stride() {
        let luma: Vec<u8> = (0..16).collect();
        let chroma: Vec<u8> = (100..108).collect();
        let mut y = [0u8; 12];
        let mut uv = [0u8; 6];
        let mut destinations = [
            PlaneDestination {
                bytes: &mut y,
                bytes_per_row: 6,
                rows: 2,
            },
            PlaneDestination {
                bytes: &mut uv,
                bytes_per_row: 6,
                rows: 1,
            },
        ];
        copy_biplanar(
            PixelFormat::Nv12VideoRange,
            4,
            2,
            [(&luma, 8), (&chroma, 8)],
            &mut destinations,
        );
        assert_eq!(y, [0, 1, 2, 3, 0, 0, 8, 9, 10, 11, 0, 0]);
        assert_eq!(uv, [100, 101, 102, 103, 0, 0]);
    }

    /// At an odd width each chroma row holds a pair for the last luma
    /// sample too, and its Cr is copied with it: 3 samples wide is 4 bytes
    /// of CbCr, 8 at 16 bits.
    #[test]
    fn an_odd_width_keeps_each_chroma_rows_last_cr() {
        let luma: Vec<u8> = (0..9).collect();
        let chroma: Vec<u8> = (100..108).collect();
        let mut y = [0u8; 12];
        let mut uv = [0u8; 8];
        let mut destinations = [
            PlaneDestination {
                bytes: &mut y,
                bytes_per_row: 4,
                rows: 3,
            },
            PlaneDestination {
                bytes: &mut uv,
                bytes_per_row: 4,
                rows: 2,
            },
        ];
        copy_biplanar(
            PixelFormat::Nv12VideoRange,
            3,
            3,
            [(&luma, 3), (&chroma, 4)],
            &mut destinations,
        );
        assert_eq!(y, [0, 1, 2, 0, 3, 4, 5, 0, 6, 7, 8, 0]);
        assert_eq!(uv, [100, 101, 102, 103, 104, 105, 106, 107]);

        let luma = vec![1u8; 6];
        let chroma: Vec<u8> = (0..8).collect();
        let mut y = [0u8; 6];
        let mut uv = [0u8; 8];
        let mut destinations = [
            PlaneDestination {
                bytes: &mut y,
                bytes_per_row: 6,
                rows: 1,
            },
            PlaneDestination {
                bytes: &mut uv,
                bytes_per_row: 8,
                rows: 1,
            },
        ];
        copy_biplanar(
            PixelFormat::P010VideoRange,
            3,
            1,
            [(&luma, 6), (&chroma, 8)],
            &mut destinations,
        );
        assert_eq!(uv, [0, 1, 2, 3, 4, 5, 6, 7]);
    }
}
