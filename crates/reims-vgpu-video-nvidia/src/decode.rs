//! The host session's decoders on NVDEC: core's [`DecoderBackend`] over
//! the codec crate's decoders, with NVDEC as their accelerator, so that the
//! guest's VideoToolbox decodes on the host's NVIDIA GPU.
//!
//! Core asks for capabilities when the guest lists decoders, opens a
//! decoder when the guest starts a decode session, and copies each frame
//! into the guest's pixel buffer: by the GPU, through
//! [`DecodedFrame::write_to_guest`] (`crate::zero_copy`), or by the CPU
//! out of the staging memory [`DecodedFrame::read_back`] fills. Frames
//! come out in decode order, as VideoToolbox's own decoders emit them. The
//! frames of one decode call wait for the GPU until one deadline, which a
//! VP9 decode's own wait shares (`crate::wait::Deadline`): core copies them
//! before it answers the call's sample. What
//! is listed comes from `cuvidGetDecoderCaps`: a codec is offered at the
//! bit depths NVDEC decodes it at 4:2:0, up to the largest size it answers.
//!
//! **A frame NVDEC fails.** NVDEC's status of a picture's decode is read at
//! its copy, after the frame came out of the codec crate's decoder, and a
//! failed one is answered as bad data (-12909). An H.264, HEVC or VP9
//! session then has its decoder fail each picture after it until an I
//! picture (H.264), an IRAP picture (HEVC) or a key frame (VP9) decodes,
//! as Apple's AVD answers a truncated slice and the frames after it, and
//! the VP9 frames after one it refuses ([`Failures`]). Core copies each
//! of the session's frames before it decodes the next, since NVIDIA's
//! decoders do not decode while copying (`decodes_while_copying`), so that
//! next decode is the frame after the failed one. A frame core does not
//! copy, as one the guest gave up on, has its status read by no one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use reims_vgpu_video_codec::av1::{
    Decoder as Av1Decoder, Output as Av1Output, av1c::ConfigurationRecord as Av1c,
    sequence::SequenceHeader,
};
use reims_vgpu_video_codec::h264::{
    self, Decoder as H264Decoder, Output as H264Output, avcc::DecoderConfiguration, sps::Sps,
};
use reims_vgpu_video_codec::hevc::{
    self, Decoder as HevcDecoder, Output as HevcOutput,
    hvcc::DecoderConfiguration as Hvcc,
    nal::{NalUnit, nal_type},
    sps::Sps as HevcSps,
};
use reims_vgpu_video_codec::vp9::{
    Decoder as Vp9Decoder, Output as Vp9Output, vpcc::ConfigurationRecord as Vpcc,
};
use reims_vgpu_video_core::backend::{
    BackendDecoder, BackendError, Codec, CodecCapabilities, CompressedFrame, DecodedFrame,
    DecoderBackend, DecoderConfig, DeviceWrite, FrameDecoded, FrameReadBack, PixelFormat,
    ProfileSupport,
};
use reims_vgpu_video_core::device_memory::GuestTarget;
use reims_vgpu_video_core::runtime::Runtime;
use reims_vgpu_video_core::surfaces::PlaneDestination;

use crate::cuda::Gpu;
use crate::error::Error;
use crate::nvdec::av1::NvdecAv1;
use crate::nvdec::decoder::{Crop, HostPlane, Picture, PictureReadBack};
use crate::nvdec::h264::NvdecH264;
use crate::nvdec::hevc::NvdecHevc;
use crate::nvdec::vp9::NvdecVp9;
use crate::nvdec::{ChromaFormat, DecodeCapabilities, NvdecCodec, decode_capabilities};
use crate::wait::Deadline;

/// The status the guest sees: what the stream asks for that the decoder or
/// driver lacks is an unsupported format, a stream that does not decode,
/// here or on NVDEC, is bad data, a RASL picture skipped is Apple's skipped
/// leading picture, and the rest is the decoder's malfunction.
pub(crate) fn backend_error(error: Error) -> BackendError {
    match error {
        Error::Unsupported(text) => BackendError::Unsupported(text),
        Error::Bitstream(text) | Error::DecodeFailed(text) => BackendError::BadData(text),
        Error::SkippedLeadingPicture(text) => BackendError::SkippedLeadingPicture(text),
        other => BackendError::Malfunction(other.to_string()),
    }
}

/// What NVDEC answers for `codec` at 4:2:0 and `bit_depth`, where it
/// decodes it.
fn supported(gpu: &Gpu, codec: NvdecCodec, bit_depth: u32) -> Option<DecodeCapabilities> {
    match decode_capabilities(gpu, codec, ChromaFormat::Yuv420, bit_depth) {
        Ok(caps) if caps.supported => Some(caps),
        Ok(_) => None,
        Err(e) => {
            log::warn!("NVDEC's caps for {codec:?} at {bit_depth} bits: {e}");
            None
        }
    }
}

/// A codec as listed: core's, NVDEC's, and its profiles as the bitstream
/// numbers them, each with the bit depths NVDEC must decode for it to be
/// listed.
struct Listing {
    codec: Codec,
    nvdec: NvdecCodec,
    profiles: &'static [(u32, &'static [u32])],
}

/// What the codec crate decodes: H.264's profile_idc at 8 bits (the codec
/// crate refuses the others), and HEVC's general_profile_idc: Main, Main 10
/// and Main Still Picture, which is Main limited to one picture and decodes
/// as Main. Apple lists HEVC's 1 to 4; the codec crate refuses the format
/// range extensions (4). VP9's Profile 0 at 8 bits and Profile 2 at 10, at
/// 4:2:0; the guest is told no VP9 profile, as Apple's host tells it none,
/// but whether VP9 is listed at all. AV1's Main (`seq_profile` 0) where
/// NVDEC decodes both its depths, 8 and 10 bits, since the host lists both
/// for it.
const LISTINGS: [Listing; 4] = [
    Listing {
        codec: Codec::H264,
        nvdec: NvdecCodec::H264,
        profiles: &[(66, &[8]), (77, &[8]), (100, &[8])],
    },
    Listing {
        codec: Codec::Hevc,
        nvdec: NvdecCodec::Hevc,
        profiles: &[(1, &[8]), (2, &[10]), (3, &[8])],
    },
    Listing {
        codec: Codec::Vp9,
        nvdec: NvdecCodec::Vp9,
        profiles: &[(0, &[8]), (2, &[10])],
    },
    Listing {
        codec: Codec::Av1,
        nvdec: NvdecCodec::Av1,
        profiles: &[(0, &[8, 10])],
    },
];

/// The codecs listed, each with its profiles and the largest picture: what
/// the codec crate decodes, where NVDEC does too.
fn capabilities(gpu: &Gpu) -> Vec<CodecCapabilities> {
    let mut listed = Vec::new();
    for Listing {
        codec,
        nvdec,
        profiles,
    } in LISTINGS
    {
        let mut offered = Vec::new();
        let (mut max_width, mut max_height) = (0, 0);
        for &(profile, bit_depths) in profiles {
            let caps: Option<Vec<DecodeCapabilities>> = bit_depths
                .iter()
                .map(|&bit_depth| supported(gpu, nvdec, bit_depth))
                .collect();
            let Some(caps) = caps else {
                continue;
            };
            // NVDEC reports no level: the host lists what Apple's decoder
            // lists for a profile without one.
            offered.push(ProfileSupport {
                profile,
                max_level: None,
            });
            for caps in caps {
                max_width = max_width.max(caps.max_width);
                max_height = max_height.max(caps.max_height);
            }
        }
        if !offered.is_empty() {
            listed.push(CodecCapabilities {
                codec,
                profiles: offered,
                max_width,
                max_height,
            });
        }
    }
    listed
}

/// The decoders of NVDEC on one GPU.
pub struct NvidiaDecoders {
    gpu: Arc<Gpu>,
    capabilities: Vec<CodecCapabilities>,
}

impl NvidiaDecoders {
    /// The decoders on `gpu`. What NVDEC decodes is asked now, once. The
    /// process's registrations of guest buffers, which their frames are
    /// written into, drop on a thread of `runtime`'s, and the crate's waits
    /// on the GPU run on its clock, unless decoders made before these made
    /// them (`zero_copy`, `wait`).
    pub fn new(gpu: Arc<Gpu>, runtime: &Arc<dyn Runtime>) -> Self {
        crate::wait::use_runtime(runtime);
        crate::zero_copy::make_registrations(runtime);
        Self {
            capabilities: capabilities(&gpu),
            gpu,
        }
    }
}

impl DecoderBackend for NvidiaDecoders {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        self.capabilities.clone()
    }

    fn open(&self, config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        if !self.capabilities.iter().any(|c| c.codec == config.codec) {
            return Err(BackendError::Unsupported(format!(
                "NVDEC here decodes no {:?}",
                config.codec
            )));
        }
        match config.codec {
            Codec::H264 => Ok(Box::new(H264Session::new(&self.gpu, config)?)),
            Codec::Hevc => Ok(Box::new(HevcSession::new(&self.gpu, config)?)),
            Codec::Vp9 => Ok(Box::new(Vp9Session::new(&self.gpu, config)?)),
            Codec::Av1 => Ok(Box::new(Av1Session::new(&self.gpu, config)?)),
        }
    }
}

fn nv12(full_range: bool) -> PixelFormat {
    if full_range {
        PixelFormat::Nv12FullRange
    } else {
        PixelFormat::Nv12VideoRange
    }
}

/// The CoreVideo format of frames at `bit_depth`, in their range: NVDEC's
/// P016 holds 10-bit samples in each 16-bit sample's high bits, as P010
/// does.
fn pixel_format(bit_depth: u32, full_range: bool) -> PixelFormat {
    match (bit_depth > 8, full_range) {
        (false, _) => nv12(full_range),
        (true, false) => PixelFormat::P010VideoRange,
        (true, true) => PixelFormat::P010FullRange,
    }
}

/// Whether NVDEC failed the decode of a frame an H.264 or HEVC session
/// gave out ([`Error::DecodeFailed`], read at the frame's copy), since the
/// session's last decode: the frame sets it, and the session's next decode
/// takes it and has the codec crate's decoder fail the pictures after it
/// (module docs).
#[derive(Clone, Default)]
pub(crate) struct Failures(Arc<AtomicBool>);

impl Failures {
    /// `error`, noted where it is a decode NVDEC failed.
    pub(crate) fn noted(&self, error: Error) -> Error {
        if matches!(error, Error::DecodeFailed(_)) {
            self.0.store(true, Ordering::Release);
        }
        error
    }

    /// Whether a decode was noted as failed since the last call.
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::Acquire)
    }
}

/// One H.264 decode session.
struct H264Session {
    gpu: Arc<Gpu>,
    decoder: H264Decoder<NvdecH264>,
    configuration: Vec<u8>,
    full_range: bool,
    failures: Failures,
}

impl H264Session {
    fn new(gpu: &Arc<Gpu>, config: &DecoderConfig) -> Result<Self, BackendError> {
        Self::open(gpu, &config.configuration).map_err(backend_error)
    }

    /// A session for an `avcC` record, whose first SPS is checked against
    /// what the codec crate and NVDEC decode, so that a stream they cannot
    /// is refused here.
    fn open(gpu: &Arc<Gpu>, avcc: &[u8]) -> Result<Self, Error> {
        let decoder = H264Decoder::from_avcc(NvdecH264::new(Arc::clone(gpu)), avcc)?;
        let record = DecoderConfiguration::parse(avcc)?;
        let sps = match record.sequence_parameter_sets.first() {
            Some(&sps) => Some(Sps::parse(sps)?),
            None => None,
        };
        if let Some(sps) = &sps {
            h264::check_supported(sps)?;
            // Opening the decoder asks NVDEC for the stream's size.
            crate::nvdec::decoder::Decoder::new(
                Arc::clone(gpu),
                &crate::nvdec::h264::spec(sps, 1),
            )?;
        }
        Ok(Self {
            gpu: Arc::clone(gpu),
            decoder,
            configuration: avcc.to_vec(),
            full_range: sps.is_some_and(|sps| sps.is_full_range()),
            failures: Failures::default(),
        })
    }
}

impl BackendDecoder for H264Session {
    fn output_format(&self) -> PixelFormat {
        nv12(self.full_range)
    }

    fn decode(
        &mut self,
        frame: &CompressedFrame<'_>,
    ) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        if let Some(config) = frame.new_config
            && config.configuration != self.configuration
        {
            // A new avcC: its parameter sets replace the old ones from this
            // frame on, which the guest starts at a random access point.
            *self = Self::new(&self.gpu, config)?;
        }
        if self.failures.take() {
            self.decoder.mark_failed();
        }
        let frames = self
            .decoder
            .decode(frame.data, frame.frame_id.cast_unsigned())
            .map_err(backend_error)?;
        let deadline = Deadline::default();
        Ok(frames
            .into_iter()
            .map(|output| h264_frame(output, &deadline, &self.failures))
            .collect())
    }

    fn flush(&mut self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        let deadline = Deadline::default();
        Ok(self
            .decoder
            .flush()
            .into_iter()
            .map(|output| h264_frame(output, &deadline, &self.failures))
            .collect())
    }

    fn can_accept(&self, config: &DecoderConfig) -> bool {
        config.codec == Codec::H264 && Self::open(&self.gpu, &config.configuration).is_ok()
    }
}

fn h264_frame(
    output: H264Output<Picture>,
    deadline: &Deadline,
    failures: &Failures,
) -> Box<dyn DecodedFrame> {
    let (x, y, width, height) = output.crop;
    Box::new(NvidiaFrame {
        picture: output.picture,
        id: output.id,
        crop: Crop {
            x,
            y,
            width,
            height,
        },
        format: nv12(output.full_range),
        deadline: deadline.clone(),
        failures: Some(failures.clone()),
    })
}

/// One HEVC decode session.
struct HevcSession {
    gpu: Arc<Gpu>,
    decoder: HevcDecoder<NvdecHevc>,
    configuration: Vec<u8>,
    format: PixelFormat,
    failures: Failures,
}

impl HevcSession {
    fn new(gpu: &Arc<Gpu>, config: &DecoderConfig) -> Result<Self, BackendError> {
        Self::open(gpu, &config.configuration).map_err(backend_error)
    }

    /// A session for an `hvcC` record, whose first SPS is checked against
    /// what the codec crate and NVDEC decode, so that a stream they cannot
    /// is refused here. Its frames are P010 at 10 bits and NV12 at 8, in
    /// the SPS's range.
    fn open(gpu: &Arc<Gpu>, hvcc: &[u8]) -> Result<Self, Error> {
        let decoder = HevcDecoder::from_hvcc(NvdecHevc::new(Arc::clone(gpu)), hvcc)?;
        let record = Hvcc::parse(hvcc)?;
        let mut sps = None;
        for unit in &record.nal_units {
            if NalUnit::new(unit)?.unit_type() == nal_type::SPS {
                sps = Some(HevcSps::parse(unit)?);
                break;
            }
        }
        let format = match &sps {
            Some(sps) => {
                hevc::check_supported(sps)?;
                // Opening the decoder asks NVDEC for the stream's size and
                // bit depth.
                crate::nvdec::decoder::Decoder::new(
                    Arc::clone(gpu),
                    &crate::nvdec::hevc::spec(sps, 1),
                )?;
                pixel_format(
                    u32::from(sps.bit_depth_luma_minus8) + 8,
                    sps.is_full_range(),
                )
            }
            None => nv12(false),
        };
        Ok(Self {
            gpu: Arc::clone(gpu),
            decoder,
            configuration: hvcc.to_vec(),
            format,
            failures: Failures::default(),
        })
    }
}

impl BackendDecoder for HevcSession {
    fn output_format(&self) -> PixelFormat {
        self.format
    }

    fn decode(
        &mut self,
        frame: &CompressedFrame<'_>,
    ) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        if let Some(config) = frame.new_config
            && config.configuration != self.configuration
        {
            // A new hvcC: its parameter sets replace the old ones from this
            // frame on, which the guest starts at a random access point.
            *self = Self::new(&self.gpu, config)?;
        }
        if self.failures.take() {
            self.decoder.mark_failed();
        }
        let frames = self
            .decoder
            .decode(frame.data, frame.frame_id.cast_unsigned())
            .map_err(backend_error)?;
        let deadline = Deadline::default();
        Ok(frames
            .into_iter()
            .map(|output| hevc_frame(output, &deadline, &self.failures))
            .collect())
    }

    fn flush(&mut self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        let deadline = Deadline::default();
        Ok(self
            .decoder
            .flush()
            .into_iter()
            .map(|output| hevc_frame(output, &deadline, &self.failures))
            .collect())
    }

    fn can_accept(&self, config: &DecoderConfig) -> bool {
        // The output format is the session's: a record of another bit
        // depth or range needs a new one.
        config.codec == Codec::Hevc
            && Self::open(&self.gpu, &config.configuration)
                .is_ok_and(|session| session.format == self.format)
    }
}

fn hevc_frame(
    output: HevcOutput<Picture>,
    deadline: &Deadline,
    failures: &Failures,
) -> Box<dyn DecodedFrame> {
    let (x, y, width, height) = output.crop;
    Box::new(NvidiaFrame {
        picture: output.picture,
        id: output.id,
        crop: Crop {
            x,
            y,
            width,
            height,
        },
        format: pixel_format(u32::from(output.bit_depth), output.full_range),
        deadline: deadline.clone(),
        failures: Some(failures.clone()),
    })
}

/// One VP9 decode session.
struct Vp9Session {
    gpu: Arc<Gpu>,
    decoder: Vp9Decoder<NvdecVp9>,
    configuration: Vec<u8>,
    format: PixelFormat,
    failures: Failures,
}

impl Vp9Session {
    fn new(gpu: &Arc<Gpu>, config: &DecoderConfig) -> Result<Self, BackendError> {
        Self::open(gpu, config).map_err(backend_error)
    }

    /// A session for a `vpcC` record, whose profile, bit depth and
    /// subsampling are checked against what the codec crate and NVDEC
    /// decode, at the session's size, so that a stream they cannot is
    /// refused here. The record gives the frames' format, which the guest
    /// asks for before the first sample.
    fn open(gpu: &Arc<Gpu>, config: &DecoderConfig) -> Result<Self, Error> {
        if config.configuration.is_empty() {
            return Err(Error::Unsupported(
                "a vp09 description without its vpcC record".into(),
            ));
        }
        let record = Vpcc::parse(&config.configuration)?;
        let bit_depth = crate::nvdec::vp9::check_format(
            record.profile,
            record.bit_depth,
            record.chroma_subsampling.is_420(),
        )?;
        if config.width > 0 && config.height > 0 {
            // Opening the decoder asks NVDEC for the session's size.
            crate::nvdec::decoder::Decoder::new(
                Arc::clone(gpu),
                &crate::nvdec::vp9::spec(bit_depth, config.width, config.height),
            )?;
        }
        Ok(Self {
            gpu: Arc::clone(gpu),
            decoder: Vp9Decoder::new(NvdecVp9::new(Arc::clone(gpu))),
            configuration: config.configuration.clone(),
            format: pixel_format(bit_depth, record.full_range),
            failures: Failures::default(),
        })
    }
}

impl BackendDecoder for Vp9Session {
    fn output_format(&self) -> PixelFormat {
        self.format
    }

    fn decode(
        &mut self,
        frame: &CompressedFrame<'_>,
    ) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        if let Some(config) = frame.new_config
            && config.configuration != self.configuration
        {
            // A new vpcC: its stream from this frame on, which the guest
            // starts at a key frame.
            *self = Self::new(&self.gpu, config)?;
        }
        if self.failures.take() {
            self.decoder.mark_failed();
        }
        decode_vp9(
            &mut self.decoder,
            frame.data,
            frame.frame_id.cast_unsigned(),
            &self.failures,
        )
        .map_err(backend_error)
    }

    fn flush(&mut self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        // VP9 holds no frame back: each chunk shows its own frames.
        Ok(Vec::new())
    }

    fn can_accept(&self, config: &DecoderConfig) -> bool {
        config.codec == Codec::Vp9
            && Self::open(&self.gpu, config).is_ok_and(|session| session.format == self.format)
    }

    /// Each frame the sample carried, those refused included, for the
    /// session to lend the guest's buffers by, as Apple's VP9 decoder lends
    /// them; read once, so that a decode that fails before the codec
    /// crate's decoder says none.
    fn frames_decoded(&mut self) -> Vec<FrameDecoded> {
        (self.decoder.take_steps().iter())
            .map(FrameDecoded::from)
            .collect()
    }
}

/// The frames `chunk` decodes into on `decoder`, as one decode call: their
/// read backs wait until the deadline its pictures' decodes wait until
/// ([`NvdecVp9::start_call`]), so that a frame whose decode took most of it
/// is answered by then all the same.
///
/// A frame the codec crate's decoder refuses after a stalled one, before
/// it reaches NVDEC, is answered as the stall, the backend's malfunction,
/// as each frame a stalled decoder decodes is.
fn decode_vp9(
    decoder: &mut Vp9Decoder<NvdecVp9>,
    chunk: &[u8],
    id: u64,
    failures: &Failures,
) -> Result<Vec<Box<dyn DecodedFrame>>, Error> {
    let deadline = decoder.accelerator().start_call();
    let frames = decoder.decode(chunk, id).map_err(|error| {
        if error == Error::from(reims_vgpu_video_codec::vp9::Error::Unrecovered) {
            decoder
                .accelerator()
                .check_not_stalled()
                .err()
                .unwrap_or(error)
        } else {
            error
        }
    })?;
    Ok(frames
        .into_iter()
        .map(|output| vp9_frame(output, &deadline, failures))
        .collect())
}

fn vp9_frame(
    output: Vp9Output<Picture>,
    deadline: &Deadline,
    failures: &Failures,
) -> Box<dyn DecodedFrame> {
    Box::new(NvidiaFrame {
        picture: output.picture,
        id: output.id,
        crop: Crop {
            x: 0,
            y: 0,
            width: output.width,
            height: output.height,
        },
        format: pixel_format(u32::from(output.bit_depth), output.full_range),
        deadline: deadline.clone(),
        failures: Some(failures.clone()),
    })
}

/// One AV1 decode session.
struct Av1Session {
    gpu: Arc<Gpu>,
    decoder: Av1Decoder<NvdecAv1>,
    configuration: Vec<u8>,
    format: PixelFormat,
}

impl Av1Session {
    /// A session for `config`, or the guest's refusal. A description NVDEC
    /// would decode, but with a side it decodes wrongly, is refused with
    /// kVTVideoDecoderMalfunctionErr, as Apple's AV1 decoder refuses a size
    /// it does not decode; one NVDEC does not decode at all stays
    /// unsupported.
    fn new(gpu: &Arc<Gpu>, config: &DecoderConfig) -> Result<Self, BackendError> {
        let session = Self::open(gpu, config).map_err(backend_error)?;
        if crate::nvdec::av1::decodes_wrongly(config.width, config.height) {
            return Err(BackendError::Malfunction(format!(
                "AV1 at {}x{}: NVDEC decodes a side of 4n+3 wrongly",
                config.width, config.height
            )));
        }
        Ok(session)
    }

    /// A session for an `av1C` record. Its sequence header, or failing one
    /// the record's own fields, is checked against what the codec crate
    /// and NVDEC decode, so that a stream they cannot is refused here: at
    /// the largest frame the sequence allows, or at the description's size
    /// where NVDEC does not take that. The record gives the frames'
    /// format, which the guest asks for before the first sample.
    fn open(gpu: &Arc<Gpu>, config: &DecoderConfig) -> Result<Self, Error> {
        let av1c = config.configuration.as_slice();
        if av1c.is_empty() {
            return Err(Error::Unsupported(
                "an av01 description without its av1C record".into(),
            ));
        }
        let decoder = Av1Decoder::from_av1c(NvdecAv1::new(Arc::clone(gpu)), av1c)?;
        let record = Av1c::parse(av1c)?;
        let sequence = match record.sequence_header_obu()? {
            Some(obu) => Some(SequenceHeader::parse(obu)?),
            None => None,
        };
        match &sequence {
            Some(sequence) => {
                // Opening the decoder asks NVDEC for the sequence's size.
                let spec = crate::nvdec::av1::spec(gpu, sequence, (config.width, config.height))?;
                crate::nvdec::decoder::Decoder::new(Arc::clone(gpu), &spec)?;
            }
            None => {
                crate::nvdec::av1::check_format(
                    record.seq_profile,
                    record.bit_depth(),
                    record.monochrome,
                    record.chroma_subsampling_x && record.chroma_subsampling_y,
                )?;
            }
        }
        let full_range = sequence.is_some_and(|sequence| sequence.color.color_range);
        Ok(Self {
            gpu: Arc::clone(gpu),
            decoder,
            configuration: av1c.to_vec(),
            format: pixel_format(u32::from(record.bit_depth()), full_range),
        })
    }
}

impl BackendDecoder for Av1Session {
    fn output_format(&self) -> PixelFormat {
        self.format
    }

    fn decode(
        &mut self,
        frame: &CompressedFrame<'_>,
    ) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        if let Some(config) = frame.new_config
            && config.configuration != self.configuration
        {
            // A new av1C: its sequence from this frame on, which the guest
            // starts at a key frame.
            *self = Self::new(&self.gpu, config)?;
        }
        let frames = self
            .decoder
            .decode(frame.data, frame.frame_id.cast_unsigned())
            .map_err(backend_error)?;
        // A frame NVDEC decoded wrongly is decoded all the same, so that
        // the frames after it have their references, and refused, as core
        // refuses a VP9 frame of another size (kVTVideoDecoderBadDataErr).
        if let Some(wrong) = frames
            .iter()
            .find(|output| self.decoder.accelerator().decoded_wrongly(&output.picture))
        {
            return Err(BackendError::BadData(format!(
                "an AV1 frame of {}x{}, or predicted from one with a side of 4n+3, \
                 which NVDEC decodes wrongly",
                wrong.width, wrong.height
            )));
        }
        let deadline = Deadline::default();
        Ok(frames
            .into_iter()
            .map(|output| av1_frame(output, &deadline))
            .collect())
    }

    fn flush(&mut self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        // AV1 holds no frame back: each temporal unit shows its own.
        Ok(Vec::new())
    }

    fn can_accept(&self, config: &DecoderConfig) -> bool {
        config.codec == Codec::Av1
            && !crate::nvdec::av1::decodes_wrongly(config.width, config.height)
            && Self::open(&self.gpu, config).is_ok_and(|session| session.format == self.format)
    }

    /// Each frame the temporal unit carried, for the session to lend the
    /// guest's buffers by, as Apple's AV1 decoder lends them.
    fn frames_decoded(&mut self) -> Vec<FrameDecoded> {
        self.decoder
            .steps()
            .iter()
            .map(FrameDecoded::from)
            .collect()
    }
}

fn av1_frame(output: Av1Output<Picture>, deadline: &Deadline) -> Box<dyn DecodedFrame> {
    Box::new(NvidiaFrame {
        picture: output.picture,
        id: output.id,
        crop: Crop {
            x: 0,
            y: 0,
            width: output.width,
            height: output.height,
        },
        format: pixel_format(u32::from(output.bit_depth), output.full_range),
        deadline: deadline.clone(),
        failures: None,
    })
}

/// A decoded frame, still on its NVDEC surface until copied.
pub(crate) struct NvidiaFrame {
    pub(crate) picture: Arc<Picture>,
    pub(crate) id: u64,
    pub(crate) crop: Crop,
    pub(crate) format: PixelFormat,
    /// The deadline of the decode call that gave it, which its read backs
    /// wait until.
    pub(crate) deadline: Deadline,
    /// Its H.264, HEVC or VP9 session's, which its copy notes a failed
    /// decode in; `None` for AV1, whose decoder keeps no such state.
    pub(crate) failures: Option<Failures>,
}

impl NvidiaFrame {
    /// `error`, noted in the session's [`Failures`] where it has them.
    pub(crate) fn noted(&self, error: Error) -> Error {
        match &self.failures {
            Some(failures) => failures.noted(error),
            None => error,
        }
    }
}

impl DecodedFrame for NvidiaFrame {
    fn frame_id(&self) -> i64 {
        self.id.cast_signed()
    }

    fn pixel_format(&self) -> PixelFormat {
        self.format
    }

    fn width(&self) -> u32 {
        self.crop.width
    }

    fn height(&self) -> u32 {
        self.crop.height
    }

    /// Maps the picture, which waits for its decode, and copies it into the
    /// decoder's staging memory; the copy out of it is the CPU's.
    fn read_back(&self) -> Result<Box<dyn FrameReadBack + '_>, BackendError> {
        let read = self
            .picture
            .read_back(self.crop, &self.deadline)
            .map_err(|error| backend_error(self.noted(error)))?;
        Ok(Box::new(NvidiaReadBack(read)))
    }

    /// Copies the picture by the GPU straight into the guest's buffer,
    /// registered with CUDA once per mapping (`crate::zero_copy`).
    fn write_to_guest(&self, target: &GuestTarget<'_>) -> Result<DeviceWrite, BackendError> {
        crate::zero_copy::write(self, target)
    }
}

/// A frame read back into its decoder's staging memory.
struct NvidiaReadBack<'a>(PictureReadBack<'a>);

impl FrameReadBack for NvidiaReadBack<'_> {
    fn copy_to(&self, planes: &mut [PlaneDestination<'_>]) -> Result<(), BackendError> {
        let [luma, chroma, ..] = planes else {
            return Err(BackendError::Malfunction(format!(
                "4:2:0 needs two destination planes, not {}",
                planes.len()
            )));
        };
        self.0.copy_to(
            HostPlane {
                bytes: luma.bytes,
                bytes_per_row: luma.bytes_per_row,
                rows: luma.rows,
            },
            HostPlane {
                bytes: chroma.bytes,
                bytes_per_row: chroma.bytes_per_row,
                rows: chroma.rows,
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decode_nvdec_failed_is_noted_once_and_answered_as_bad_data() {
        let failures = Failures::default();
        let error = failures.noted(Error::DecodeFailed("status 8".to_owned()));
        assert!(matches!(backend_error(error), BackendError::BadData(_)));
        assert!(failures.take());
        assert!(!failures.take(), "taken twice");
    }

    #[test]
    fn other_errors_are_not_noted() {
        let failures = Failures::default();
        for error in [
            Error::Bitstream("a slice header".to_owned()),
            Error::Stalled("a decode".to_owned()),
            Error::OutOfSurfaces,
            Error::Unsupported("4:2:2".to_owned()),
        ] {
            let _ = failures.noted(error);
        }
        assert!(!failures.take());
    }

    #[test]
    fn a_sessions_frames_note_into_its_failures() {
        let failures = Failures::default();
        let frame = failures.clone();
        let _ = frame.noted(Error::DecodeFailed("status 9".to_owned()));
        assert!(failures.take());
    }
}

/// A picture's work past its deadline, on an NVIDIA GPU, of H.264 that
/// NVENC encodes first, and of VP9 that ffmpeg's libvpx encodes
/// (`crate::wait`), and a picture NVDEC fails: run on the NVIDIA host.
#[cfg(test)]
mod gpu {
    use std::rc::Rc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use reims_vgpu_video_codec::encode::{h264 as written, nal};
    use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
    use reims_vgpu_video_core::surfaces::PlaneDestination;
    use reims_vgpu_video_core::wire::message::MediaTime;

    use super::*;
    use crate::cuda::GpuSelector;
    use crate::cuda::spin::{HELD_AT_MOST, Spin};
    use crate::nvdec::decoder::tests::{BEFORE_MAP, BEFORE_SUBMIT, PAGEABLE, TOUCHED_IN_FLIGHT};
    use crate::nvenc::encoder::tests::{noise, params};
    use crate::nvenc::encoder::{EncodeParams, EncodeProfile, NvencEncoder, RawFrame};
    use crate::wait::tests::{KEPT_EVER, WAIT, within_5_s};

    /// `count` frames of noise at `width` by `height`, H.264 by NVENC with
    /// no B pictures: its avcC, and its samples, length-prefixed, with no
    /// parameter sets.
    fn h264(gpu: &Arc<Gpu>, size: (u32, u32), count: u64) -> (Vec<u8>, Vec<Vec<u8>>) {
        h264_with_idrs(gpu, size, count, &[0])
    }

    /// [`h264`]'s stream, with an IDR picture at each of `idrs`, and no
    /// other I picture among the first 30.
    fn h264_with_idrs(
        gpu: &Arc<Gpu>,
        (width, height): (u32, u32),
        count: u64,
        idrs: &[u64],
    ) -> (Vec<u8>, Vec<Vec<u8>>) {
        let params = EncodeParams {
            width,
            height,
            b_frames: 0,
            ..params(EncodeProfile::H264High)
        };
        let mut encoder = NvencEncoder::new(Arc::clone(gpu), &params).expect("an NVENC session");
        let sets = nal::split_annex_b(encoder.sequence_parameters());
        let [sps, pps] = sets[..] else {
            panic!("NVENC's parameter sets are {} units", sets.len());
        };
        let avcc = written::avcc(sps, pps);
        let mut samples = Vec::new();
        for n in 0..count {
            let (luma, chroma, row) = noise(&params, n);
            let packets = encoder
                .encode(&RawFrame {
                    luma: (&luma, row),
                    chroma: (&chroma, row),
                    index: n,
                    force_idr: idrs.contains(&n),
                })
                .expect("a frame NVENC takes");
            for packet in packets {
                let packet = packet.expect("a packet");
                let mut sample = Vec::new();
                for unit in nal::split_annex_b(&packet.data) {
                    if !matches!(unit.first().map(|b| b & 0x1f), Some(7 | 8)) {
                        nal::push_length_prefixed(&mut sample, unit);
                    }
                }
                samples.push(sample);
            }
        }
        (avcc, samples)
    }

    /// Every frame `samples` decode into, flushed at the end, each with a
    /// deadline of its own, from its first wait.
    fn decoded(session: &mut H264Session, samples: &[Vec<u8>]) -> Vec<Box<dyn DecodedFrame>> {
        let failures = session.failures.clone();
        let frame = |output| h264_frame(output, &Deadline::default(), &failures);
        let mut frames = Vec::new();
        for (id, sample) in samples.iter().enumerate() {
            let id = u64::try_from(id).unwrap_or(0);
            let out = session.decoder.decode(sample, id).expect("a decode");
            frames.extend(out.into_iter().map(frame));
        }
        frames.extend(session.decoder.flush().into_iter().map(frame));
        frames
    }

    fn stalled(missed: Option<&BackendError>) -> bool {
        matches!(missed, Some(BackendError::Malfunction(text)) if text.contains("did not finish"))
    }

    /// `sample`, its one slice cut to `percent` per cent of its bytes and
    /// its 4-byte length rewritten: a truncated slice, whose header our
    /// parser reads whole.
    fn truncated(sample: &[u8], percent: usize) -> Vec<u8> {
        let (prefix, unit) = sample.split_at(4);
        let len = u32::from_be_bytes(prefix.try_into().expect("4 bytes"));
        let len = usize::try_from(len).expect("a length");
        assert_eq!(len, unit.len(), "one NAL unit a sample");
        let kept = (len * percent / 100).max(16);
        let mut out = u32::try_from(kept)
            .expect("a length")
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(&unit[..kept]);
        out
    }

    /// A truncated P picture's slice reaches NVDEC, whose status fails the
    /// picture at its copy; the session then fails each picture after it
    /// until the next IDR picture, as Apple's AVD answers a truncated
    /// slice, and the frames from that IDR picture on read back.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn a_picture_nvdec_fails_fails_each_after_it_until_an_i_picture() {
        let gpu = Arc::new(Gpu::open(&GpuSelector::First).expect("an NVIDIA GPU"));
        let (avcc, mut samples) = h264_with_idrs(&gpu, (640, 360), 20, &[0, 12]);
        samples[5] = truncated(&samples[5], 50);
        let mut session = H264Session::open(&gpu, &avcc).expect("a session");
        let mut failed = Vec::new();
        for (id, sample) in samples.iter().enumerate() {
            let frame = CompressedFrame {
                frame_id: i64::try_from(id).expect("an id"),
                data: sample,
                sample_sizes: &[],
                presentation_time: MediaTime::INVALID,
                decode_time: MediaTime::INVALID,
                duration: MediaTime::INVALID,
                new_config: None,
            };
            let read = session.decode(&frame).and_then(|frames| {
                assert_eq!(frames.len(), 1, "frame {id}");
                frames[0].read_back().map(drop)
            });
            match read {
                Ok(()) => {}
                Err(BackendError::BadData(_)) => failed.push(id),
                Err(other) => panic!("frame {id}: {other}"),
            }
        }
        assert_eq!(failed, (5..12).collect::<Vec<_>>());
    }

    /// A picture whose copies the GPU holds back, behind a kernel that
    /// spins until the test lets it go, fails as the backend's malfunction
    /// at its deadline; the decoder fails each later read back at once,
    /// and the picture, mapped, with the staging memory its copies write,
    /// is kept until the GPU is done, then let go.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn a_copy_the_gpu_holds_past_its_deadline_is_kept_until_it_is_done() {
        let gpu = Arc::new(Gpu::open(&GpuSelector::First).expect("an NVIDIA GPU"));
        let (avcc, samples) = h264(&gpu, (1920, 1080), 3);
        let mut session = H264Session::open(&gpu, &avcc).expect("a decoder");
        let frames = decoded(&mut session, &samples);
        assert_eq!(frames.len(), 3);
        assert!(
            frames[0].read_back().is_ok(),
            "the first frame did not read back"
        );
        let spin = Rc::new(Spin::new(&gpu).expect("the spin kernel"));
        let holds = Rc::clone(&spin);
        BEFORE_MAP.set(Some(Box::new(move |stream| {
            holds.hold(stream).expect("the spin queued");
        })));
        WAIT.set(Some(Duration::from_millis(100)));
        let kept = KEPT_EVER.load(Ordering::SeqCst);
        let start = StdRuntime.now();
        let missed = frames[1].read_back().err();
        let waited = StdRuntime.elapsed_since(start);
        assert!(stalled(missed.as_ref()), "{missed:?}");
        assert!(
            waited >= Duration::from_millis(100) && waited < HELD_AT_MOST,
            "the read back waited {waited:?}"
        );
        let start = StdRuntime.now();
        let later = frames[2].read_back().err();
        assert!(stalled(later.as_ref()), "{later:?}");
        assert!(StdRuntime.elapsed_since(start) < Duration::from_millis(5));
        WAIT.set(None);
        assert_eq!(KEPT_EVER.load(Ordering::SeqCst), kept + 1);
        std::thread::sleep(Duration::from_millis(300));
        assert!(crate::wait::kept() >= 1, "let go while the GPU holds it");
        spin.release();
        assert!(within_5_s(|| crate::wait::kept() == 0), "still kept");
    }

    /// A picture whose decode is still running at its deadline, here none
    /// at all, fails the same way, and is kept until NVDEC has decoded it.
    /// The decode may finish before its read back asks, so each try opens
    /// a decoder of its own.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn a_decode_still_running_at_its_deadline_is_kept_until_it_is_done() {
        let gpu = Arc::new(Gpu::open(&GpuSelector::First).expect("an NVIDIA GPU"));
        let (avcc, samples) = h264(&gpu, (3840, 2160), 1);
        for _ in 0..20 {
            let mut session = H264Session::open(&gpu, &avcc).expect("a decoder");
            let kept = KEPT_EVER.load(Ordering::SeqCst);
            WAIT.set(Some(Duration::ZERO));
            let frames = decoded(&mut session, &samples);
            let missed = frames.first().and_then(|frame| frame.read_back().err());
            WAIT.set(None);
            match missed {
                None => continue,
                missed if stalled(missed.as_ref()) => {
                    assert_eq!(KEPT_EVER.load(Ordering::SeqCst), kept + 1);
                    drop(frames);
                    drop(session);
                    assert!(within_5_s(|| crate::wait::kept() == 0), "still kept");
                    return;
                }
                missed => panic!("{missed:?}"),
            }
        }
        panic!("every decode was done before its read back asked, in 20 tries");
    }

    /// `frame`'s planes, read back.
    fn planes(frame: &dyn DecodedFrame) -> (Vec<u8>, Vec<u8>) {
        let row = usize::try_from(frame.width()).unwrap_or(0);
        let rows = frame.height();
        let mut luma = vec![0; row * usize::try_from(rows).unwrap_or(0)];
        let mut chroma = vec![0; row * usize::try_from(rows.div_ceil(2)).unwrap_or(0)];
        let read = frame.read_back().expect("a read back");
        read.copy_to(&mut [
            PlaneDestination {
                bytes: &mut luma,
                bytes_per_row: row,
                rows,
            },
            PlaneDestination {
                bytes: &mut chroma,
                bytes_per_row: row,
                rows: rows.div_ceil(2),
            },
        ])
        .expect("a copy");
        (luma, chroma)
    }

    /// A picture read back through pageable staging memory, as where CUDA
    /// gives no page-locked memory, is the picture read back through
    /// page-locked memory. Its copies the GPU holds back, behind a kernel
    /// that spins until the test lets it go, and a copy into pageable
    /// memory returns only once done: it is made on the decoder's call
    /// thread, and the picture fails as the backend's malfunction at its
    /// deadline. The decoder fails each later read back at once, and the
    /// picture, mapped, with the staging memory, is kept until the copies
    /// have returned and the GPU is done, the decoder's handle and stream
    /// untouched meanwhile, then let go.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn a_copy_into_pageable_memory_past_its_deadline_is_kept_until_it_returns() {
        let gpu = Arc::new(Gpu::open(&GpuSelector::First).expect("an NVIDIA GPU"));
        let (avcc, samples) = h264(&gpu, (1920, 1080), 3);
        let mut session = H264Session::open(&gpu, &avcc).expect("a decoder");
        let pinned = planes(&*decoded(&mut session, &samples)[0]);
        drop(session);
        PAGEABLE.set(true);
        let mut session = H264Session::open(&gpu, &avcc).expect("a decoder");
        let frames = decoded(&mut session, &samples);
        assert_eq!(frames.len(), 3);
        assert!(
            planes(&*frames[0]) == pinned,
            "the frame read back through pageable memory is not the frame read back through \
             page-locked memory"
        );
        let spin = Rc::new(Spin::new(&gpu).expect("the spin kernel"));
        let holds = Rc::clone(&spin);
        BEFORE_MAP.set(Some(Box::new(move |stream| {
            holds.hold(stream).expect("the spin queued");
        })));
        WAIT.set(Some(Duration::from_millis(100)));
        let kept = KEPT_EVER.load(Ordering::SeqCst);
        let touched = TOUCHED_IN_FLIGHT.load(Ordering::SeqCst);
        let start = StdRuntime.now();
        let missed = frames[1].read_back().err();
        let waited = StdRuntime.elapsed_since(start);
        assert!(stalled(missed.as_ref()), "{missed:?}");
        assert!(
            waited >= Duration::from_millis(100) && waited < HELD_AT_MOST,
            "the read back waited {waited:?}"
        );
        let start = StdRuntime.now();
        let later = frames[2].read_back().err();
        assert!(stalled(later.as_ref()), "{later:?}");
        assert!(StdRuntime.elapsed_since(start) < Duration::from_millis(5));
        WAIT.set(None);
        PAGEABLE.set(false);
        assert_eq!(KEPT_EVER.load(Ordering::SeqCst), kept + 1);
        drop(frames);
        drop(session);
        std::thread::sleep(Duration::from_millis(300));
        assert!(crate::wait::kept() >= 1, "let go while the GPU holds it");
        spin.release();
        assert!(within_5_s(|| crate::wait::kept() == 0), "still kept");
        assert_eq!(
            TOUCHED_IN_FLIGHT.load(Ordering::SeqCst),
            touched,
            "the decoder's handle or stream was used while the copies were in flight"
        );
        let (turns, spun) = spin.ran();
        eprintln!("the spin read its flag {turns} times in {spun:?}");
    }

    /// `count` frames of `testsrc2` at `width` by `height`, VP9 Profile 0 by
    /// ffmpeg's libvpx, with no hidden frames: each chunk of its IVF.
    fn vp9((width, height): (u32, u32), count: u32) -> Vec<Vec<u8>> {
        let dir = std::env::temp_dir().join(format!("rvg-nvidia-vp9-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("stream.ivf");
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={width}x{height}:rate=30"))
            .args(["-frames:v", &count.to_string(), "-pix_fmt", "yuv420p"])
            .args([
                "-c:v",
                "libvpx-vp9",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
            ])
            .args(["-lag-in-frames", "0", "-auto-alt-ref", "0", "-b:v", "20M"])
            .arg(&path)
            .status()
            .expect("ffmpeg");
        assert!(status.success(), "ffmpeg: {status}");
        let file = std::fs::read(&path).expect("the stream");
        std::fs::remove_dir_all(&dir).expect("the scratch directory removed");
        let chunks = reims_vgpu_video_conformance::ivf::vp9_frames(&file).expect("an IVF file");
        chunks
            .into_iter()
            .map(|(at, len)| file[at..at + len].to_vec())
            .collect()
    }

    /// A VP9 picture whose decode is still running at its deadline, here
    /// none at all, fails as the backend's malfunction: its
    /// `cuvidDecodePicture` returns only once the picture is decoded, and
    /// is made on the decoder's call thread. The decoder fails each later
    /// decode at once, and the call is kept with the decoder until it has
    /// returned, the decoder's handle and stream untouched meanwhile, and
    /// the decoder destroyed after.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn a_vp9_decode_still_running_at_its_deadline_is_kept_until_it_returns() {
        use crate::nvdec::vp9::NvdecVp9;

        let gpu = Arc::new(Gpu::open(&GpuSelector::First).expect("an NVIDIA GPU"));
        let chunks = vp9((3840, 2160), 4);
        let mut decoder = Vp9Decoder::new(NvdecVp9::new(Arc::clone(&gpu)));
        let first =
            decode_vp9(&mut decoder, &chunks[0], 0, &Failures::default()).expect("the key frame");
        assert!(
            first[0].read_back().is_ok(),
            "the key frame did not read back"
        );
        WAIT.set(Some(Duration::ZERO));
        let kept = KEPT_EVER.load(Ordering::SeqCst);
        let touched = TOUCHED_IN_FLIGHT.load(Ordering::SeqCst);
        let missed = decode_vp9(&mut decoder, &chunks[1], 1, &Failures::default()).err();
        assert!(matches!(&missed, Some(Error::Stalled(_))), "{missed:?}");
        assert!(stalled(missed.map(backend_error).as_ref()));
        WAIT.set(None);
        let start = StdRuntime.now();
        let later = decode_vp9(&mut decoder, &chunks[2], 2, &Failures::default()).err();
        assert!(matches!(&later, Some(Error::Stalled(_))), "{later:?}");
        assert!(StdRuntime.elapsed_since(start) < Duration::from_millis(5));
        assert!(
            matches!(
                first[0].read_back().err(),
                Some(BackendError::Malfunction(_))
            ),
            "a read back of a stalled decoder"
        );
        assert_eq!(KEPT_EVER.load(Ordering::SeqCst), kept + 1);
        drop(first);
        drop(decoder);
        assert!(within_5_s(|| crate::wait::kept() == 0), "still kept");
        assert_eq!(
            TOUCHED_IN_FLIGHT.load(Ordering::SeqCst),
            touched,
            "the decoder's handle or stream was used while its decode was in flight"
        );
    }

    /// A VP9 picture whose decode's submission does not return, held on
    /// the decoder's call thread by the test inside the submission, as a
    /// spinning kernel does not hold NVDEC's VP9 decode, fails as the
    /// backend's malfunction at its deadline. The decoder fails each later
    /// call at once, and, dropped, is kept with the call until it returns,
    /// its handle and stream untouched meanwhile, then destroyed.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn a_vp9_decode_that_does_not_return_is_kept_until_it_returns() {
        use std::sync::mpsc;

        use crate::nvdec::vp9::NvdecVp9;

        let gpu = Arc::new(Gpu::open(&GpuSelector::First).expect("an NVIDIA GPU"));
        let chunks = vp9((1280, 720), 4);
        let mut decoder = Vp9Decoder::new(NvdecVp9::new(Arc::clone(&gpu)));
        let first =
            decode_vp9(&mut decoder, &chunks[0], 0, &Failures::default()).expect("the key frame");
        assert!(
            first[0].read_back().is_ok(),
            "the key frame did not read back"
        );
        let (release, released) = mpsc::channel::<()>();
        BEFORE_SUBMIT.set(Some(Box::new(move || {
            let _ = released.recv_timeout(HELD_AT_MOST);
        })));
        WAIT.set(Some(Duration::from_millis(100)));
        let kept = KEPT_EVER.load(Ordering::SeqCst);
        let touched = TOUCHED_IN_FLIGHT.load(Ordering::SeqCst);
        let start = StdRuntime.now();
        let missed = decode_vp9(&mut decoder, &chunks[1], 1, &Failures::default()).err();
        let waited = StdRuntime.elapsed_since(start);
        assert!(matches!(&missed, Some(Error::Stalled(_))), "{missed:?}");
        assert!(stalled(missed.map(backend_error).as_ref()));
        assert!(
            waited >= Duration::from_millis(100) && waited < HELD_AT_MOST,
            "the decode waited {waited:?}"
        );
        WAIT.set(None);
        let start = StdRuntime.now();
        let later = decode_vp9(&mut decoder, &chunks[2], 2, &Failures::default()).err();
        assert!(matches!(&later, Some(Error::Stalled(_))), "{later:?}");
        assert!(
            matches!(
                first[0].read_back().err(),
                Some(BackendError::Malfunction(_))
            ),
            "a read back of a stalled decoder"
        );
        assert!(StdRuntime.elapsed_since(start) < Duration::from_millis(5));
        assert_eq!(KEPT_EVER.load(Ordering::SeqCst), kept + 1);
        drop(first);
        drop(decoder);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            crate::wait::kept() >= 1,
            "let go while the call is in flight"
        );
        release.send(()).expect("the call waits");
        assert!(within_5_s(|| crate::wait::kept() == 0), "still kept");
        assert_eq!(
            TOUCHED_IN_FLIGHT.load(Ordering::SeqCst),
            touched,
            "the decoder's handle or stream was used while its decode was in flight"
        );
    }

    /// A VP9 frame whose decode took most of its deadline, held inside its
    /// submission by the test, and whose copies the GPU then holds back,
    /// behind a kernel that spins until the test lets it go, fails as the
    /// backend's malfunction at its decode's deadline: its read back waits
    /// until the deadline of the decode call that gave it, so a frame is
    /// answered within one deadline of its first wait, where a deadline of
    /// the read back's own would answer it 0.7 s later. The picture is kept
    /// until the GPU is done, then let go.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn a_vp9_frame_is_answered_by_its_decodes_deadline() {
        use crate::nvdec::vp9::NvdecVp9;

        const DEADLINE: Duration = Duration::from_millis(1000);
        const DECODING: Duration = Duration::from_millis(700);
        let gpu = Arc::new(Gpu::open(&GpuSelector::First).expect("an NVIDIA GPU"));
        let chunks = vp9((1280, 720), 3);
        let mut decoder = Vp9Decoder::new(NvdecVp9::new(Arc::clone(&gpu)));
        let first =
            decode_vp9(&mut decoder, &chunks[0], 0, &Failures::default()).expect("the key frame");
        assert!(
            first[0].read_back().is_ok(),
            "the key frame did not read back"
        );
        BEFORE_SUBMIT.set(Some(Box::new(|| std::thread::sleep(DECODING))));
        WAIT.set(Some(DEADLINE));
        let start = StdRuntime.now();
        let second = decode_vp9(&mut decoder, &chunks[1], 1, &Failures::default())
            .expect("a decode by its deadline");
        let decoded = StdRuntime.elapsed_since(start);
        assert!(
            decoded >= DECODING && decoded < DEADLINE,
            "the decode took {decoded:?}"
        );
        let spin = Rc::new(Spin::new(&gpu).expect("the spin kernel"));
        let holds = Rc::clone(&spin);
        BEFORE_MAP.set(Some(Box::new(move |stream| {
            holds.hold(stream).expect("the spin queued");
        })));
        let kept = KEPT_EVER.load(Ordering::SeqCst);
        let missed = second[0].read_back().err();
        let answered = StdRuntime.elapsed_since(start);
        WAIT.set(None);
        assert!(stalled(missed.as_ref()), "{missed:?}");
        assert!(
            answered >= DEADLINE && answered < DEADLINE + Duration::from_millis(100),
            "the frame was answered {answered:?} after its decode began"
        );
        assert_eq!(KEPT_EVER.load(Ordering::SeqCst), kept + 1);
        drop(second);
        drop(first);
        drop(decoder);
        std::thread::sleep(Duration::from_millis(300));
        assert!(crate::wait::kept() >= 1, "let go while the GPU holds it");
        spin.release();
        assert!(within_5_s(|| crate::wait::kept() == 0), "still kept");
        eprintln!("decoded in {decoded:?}, answered in {answered:?}");
    }
}
