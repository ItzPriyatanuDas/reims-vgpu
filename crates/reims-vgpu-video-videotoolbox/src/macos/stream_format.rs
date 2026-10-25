//! What a decoder configuration asks of VideoToolbox: the CoreMedia codec
//! type and sample description atom of each codec core knows, the
//! CMVideoFormatDescription made from the guest's configuration record, and
//! the pixel format its frames decode into.
//!
//! A codec core gains is served once it is in [`CODECS`], and [`atom`]
//! names its record's atom before the crate builds.

use reims_vgpu_video_codec::{av1, h264, hevc, vp9};
use reims_vgpu_video_core::backend::{BackendError, Codec, DecoderConfig, PixelFormat};

use super::cf::{self, Owned, constant};
use super::ffi::core_media::{
    CMVideoCodecType, CMVideoFormatDescriptionCreate, CMVideoFormatDescriptionRef,
    kCMVideoCodecType_AV1, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, kCMVideoCodecType_VP9,
};
use super::ffi::video_toolbox::VTRegisterSupplementalVideoDecoderIfAvailable;
use super::status::native_error;

/// The codecs core knows, each of which VideoToolbox may decode.
pub(crate) const CODECS: [Codec; 4] = [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9];

/// The sample description extension atom that carries `codec`'s decoder
/// configuration record, as MP4 stores it (ISO/IEC 14496-15 for `avcC` and
/// `hvcC`, the AV1 ISOBMFF binding for `av1C`, VP9's codec binding for
/// `vpcC`).
fn atom(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "avcC",
        Codec::Hevc => "hvcC",
        Codec::Av1 => "av1C",
        Codec::Vp9 => "vpcC",
    }
}

/// The codec type of `codec`, as CoreMedia names it.
pub(crate) fn codec_type(codec: Codec) -> CMVideoCodecType {
    match codec {
        Codec::H264 => kCMVideoCodecType_H264,
        Codec::Hevc => kCMVideoCodecType_HEVC,
        Codec::Av1 => kCMVideoCodecType_AV1,
        Codec::Vp9 => kCMVideoCodecType_VP9,
    }
}

/// Registers VideoToolbox's decoder of `codec` where it is not registered
/// by default, as VP9's is not: without it a VP9 session does not open
/// (-12906, kVTCouldNotFindVideoDecoderErr, measured on the M3 Max,
/// 2026-10-24), and
/// VTIsHardwareDecodeSupported says no (`VTUtilities.h`). Once a process,
/// before the first listing or session.
pub(crate) fn register_decoder(codec: Codec) {
    static VP9: std::sync::Once = std::sync::Once::new();
    if codec == Codec::Vp9 {
        // SAFETY: a plain call with a codec type.
        VP9.call_once(|| unsafe {
            VTRegisterSupplementalVideoDecoderIfAvailable(codec_type(codec))
        });
    }
}

/// The CMVideoFormatDescription of `config`: its codec type and size, with
/// the configuration record as its sample description extension atom, as
/// AVFoundation makes one from an MP4's sample entry.
pub(crate) fn format_description(config: &DecoderConfig) -> Result<Owned, BackendError> {
    register_decoder(config.codec);
    let width = i32::try_from(config.width)
        .map_err(|_| BackendError::Unsupported(format!("a width of {}", config.width)))?;
    let height = i32::try_from(config.height)
        .map_err(|_| BackendError::Unsupported(format!("a height of {}", config.height)))?;
    let key = cf::string(atom(config.codec));
    let record = cf::data(&config.configuration);
    let atoms = cf::dictionary(&[(key.cf(), record.cf())]);
    let extensions = cf::dictionary(&[(
        constant!(core_media::kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms),
        atoms.cf(),
    )]);
    let mut description: CMVideoFormatDescriptionRef = std::ptr::null();
    // SAFETY: the extensions are a live dictionary, and the out pointer is
    // a live local.
    let status = unsafe {
        CMVideoFormatDescriptionCreate(
            std::ptr::null(),
            codec_type(config.codec),
            width,
            height,
            extensions.as_ptr(),
            &raw mut description,
        )
    };
    // SAFETY: a Create call's result, which the caller now owns, or NULL.
    let description = unsafe { Owned::take(description) };
    if status != 0 {
        return Err(native_error("CMVideoFormatDescriptionCreate", status));
    }
    description.ok_or_else(|| BackendError::Malfunction("no format description".into()))
}

/// The CoreVideo format 4:2:0 frames of `bit_depth` decode into, in their
/// range: NV12 for 8 bits, P010 for 9 and 10.
fn biplanar(bit_depth: u32, full_range: bool) -> Result<PixelFormat, BackendError> {
    Ok(match (bit_depth, full_range) {
        (8, false) => PixelFormat::Nv12VideoRange,
        (8, true) => PixelFormat::Nv12FullRange,
        (9 | 10, false) => PixelFormat::P010VideoRange,
        (9 | 10, true) => PixelFormat::P010FullRange,
        (depth, _) => {
            return Err(BackendError::Unsupported(format!(
                "{depth}-bit samples, which no 4:2:0 format here holds"
            )));
        }
    })
}

/// A chroma format core's 4:2:0 buffers hold: 4:2:0, or monochrome, whose
/// buffers' chroma is mid-grey.
fn check_chroma(chroma_format_idc: u8) -> Result<(), BackendError> {
    match chroma_format_idc {
        0 | 1 => Ok(()),
        2 => Err(BackendError::Unsupported("4:2:2 chroma".into())),
        _ => Err(BackendError::Unsupported("4:4:4 chroma".into())),
    }
}

/// The pixel format `config`'s stream decodes into, read from its record:
/// the bit depth and range of its first sequence parameter set (or
/// sequence header, or the `vpcC`'s fields). A record that does not read,
/// or a stream of more than 10 bits or other than 4:2:0 and monochrome,
/// is refused, since core's buffers are 4:2:0 at 8 or 10 bits and
/// VideoToolbox would convert it.
pub(crate) fn output_format(config: &DecoderConfig) -> Result<PixelFormat, BackendError> {
    let record = &config.configuration;
    let bad = |what: &str, error: &dyn std::fmt::Display| {
        BackendError::BadData(format!("the {what} record does not read: {error}"))
    };
    match &config.codec.fourcc().to_be_bytes() {
        b"avc1" => {
            let avcc = h264::avcc::DecoderConfiguration::parse(record)
                .map_err(|error| bad("avcC", &error))?;
            let sps = avcc
                .sequence_parameter_sets
                .first()
                .ok_or_else(|| BackendError::BadData("an avcC record with no SPS".into()))?;
            let sps = h264::sps::Sps::parse(sps).map_err(|error| bad("avcC's SPS", &error))?;
            check_chroma(sps.chroma_format_idc)?;
            biplanar(
                u32::from(sps.bit_depth_luma_minus8) + 8,
                sps.is_full_range(),
            )
        }
        b"hvc1" => {
            let hvcc = hevc::hvcc::DecoderConfiguration::parse(record)
                .map_err(|error| bad("hvcC", &error))?;
            let sps = hvcc
                .nal_units
                .iter()
                .find(|unit| {
                    hevc::nal::NalUnit::new(unit)
                        .is_ok_and(|nal| nal.unit_type() == hevc::nal::nal_type::SPS)
                })
                .ok_or_else(|| BackendError::BadData("an hvcC record with no SPS".into()))?;
            let sps = hevc::sps::Sps::parse(sps).map_err(|error| bad("hvcC's SPS", &error))?;
            check_chroma(sps.chroma_format_idc)?;
            biplanar(
                u32::from(sps.bit_depth_luma_minus8) + 8,
                sps.is_full_range(),
            )
        }
        b"av01" => {
            let av1c = av1::av1c::ConfigurationRecord::parse(record)
                .map_err(|error| bad("av1C", &error))?;
            if av1c.seq_profile != 0 {
                return Err(BackendError::Unsupported(format!(
                    "AV1 seq_profile {}: only Main is 4:2:0",
                    av1c.seq_profile
                )));
            }
            let full_range = av1c
                .sequence_header_obu()
                .ok()
                .flatten()
                .and_then(|obu| av1::sequence::SequenceHeader::parse(obu).ok())
                .is_some_and(|sequence| sequence.color.color_range);
            biplanar(u32::from(av1c.bit_depth()), full_range)
        }
        b"vp09" => {
            let vpcc = vp9::vpcc::ConfigurationRecord::parse(record)
                .map_err(|error| bad("vpcC", &error))?;
            if !vpcc.chroma_subsampling.is_420() {
                return Err(BackendError::Unsupported("VP9 other than 4:2:0".into()));
            }
            biplanar(u32::from(vpcc.bit_depth), vpcc.full_range)
        }
        _ => Err(BackendError::Unsupported(format!(
            "VideoToolbox does not decode {:?} here",
            config.codec
        ))),
    }
}

/// The picture size `config`'s record gives its stream: the first SPS's,
/// after its cropping, for H.264 and HEVC, and the sequence header's
/// largest frame for AV1. `None` where the record does not say, as VP9's
/// `vpcC` does not.
///
/// VideoToolbox makes its frames the format description's size, the
/// decoded picture at their top left: a description of 1x1 over a 640x360
/// stream gave 1x1 frames, and one of 1280x720 gave 1280x720 frames whose
/// top left was the picture. Apple's host writes the picture alone into
/// the guest's buffer of the description's size and leaves the rest as it
/// was, so the backend's frames are this size where it is smaller.
pub(crate) fn stream_size(config: &DecoderConfig) -> Option<(u32, u32)> {
    let record = &config.configuration;
    match &config.codec.fourcc().to_be_bytes() {
        b"avc1" => {
            let avcc = h264::avcc::DecoderConfiguration::parse(record).ok()?;
            let sps = h264::sps::Sps::parse(avcc.sequence_parameter_sets.first()?).ok()?;
            let (_, _, width, height) = sps.crop_rect();
            Some((width, height))
        }
        b"hvc1" => {
            let hvcc = hevc::hvcc::DecoderConfiguration::parse(record).ok()?;
            let sps = hvcc.nal_units.iter().find(|unit| {
                hevc::nal::NalUnit::new(unit)
                    .is_ok_and(|nal| nal.unit_type() == hevc::nal::nal_type::SPS)
            })?;
            let (_, _, width, height) = hevc::sps::Sps::parse(sps).ok()?.crop_rect();
            Some((width, height))
        }
        b"av01" => {
            let av1c = av1::av1c::ConfigurationRecord::parse(record).ok()?;
            let sequence =
                av1::sequence::SequenceHeader::parse(av1c.sequence_header_obu().ok()??).ok()?;
            Some((
                sequence.max_frame_width_minus_1.saturating_add(1),
                sequence.max_frame_height_minus_1.saturating_add(1),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each codec core knows is listed once, and the FourCC core names it
    /// by, which the guest sends, is CoreMedia's codec type for it, which
    /// the header check holds to the SDK's.
    #[test]
    fn every_codec_core_knows_is_listed_once_by_coremedias_type() {
        for codec in CODECS {
            assert_eq!(CODECS.iter().filter(|&&c| c == codec).count(), 1);
            assert_eq!(Codec::from_fourcc(codec.fourcc()), Some(codec));
            assert_eq!(codec_type(codec), codec.fourcc().0, "{codec:?}");
        }
    }

    /// The pixel formats core names its decoded frames and the scaler
    /// device's surfaces by are CoreVideo's own, which the header check
    /// holds to the SDK's.
    #[test]
    fn cores_pixel_formats_are_corevideos() {
        use reims_vgpu_video_core::scaler::format::ScalerFormat;

        use super::super::ffi::core_video::{
            kCVPixelFormatType_32ARGB, kCVPixelFormatType_32BGRA,
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange, kCVPixelFormatType_422YpCbCr8,
            kCVPixelFormatType_422YpCbCr8_yuvs,
        };

        for (format, code) in [
            (
                PixelFormat::Nv12VideoRange,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            ),
            (
                PixelFormat::Nv12FullRange,
                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            ),
            (
                PixelFormat::P010VideoRange,
                kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            ),
            (
                PixelFormat::P010FullRange,
                kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
            ),
        ] {
            assert_eq!(format.fourcc().0, code, "{format:?}");
        }
        for (format, code) in [
            (
                ScalerFormat::Nv12VideoRange,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            ),
            (
                ScalerFormat::Nv12FullRange,
                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            ),
            (
                ScalerFormat::P010VideoRange,
                kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            ),
            (
                ScalerFormat::P010FullRange,
                kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
            ),
            (ScalerFormat::Yuyv, kCVPixelFormatType_422YpCbCr8_yuvs),
            (ScalerFormat::Uyvy, kCVPixelFormatType_422YpCbCr8),
            (ScalerFormat::Bgra, kCVPixelFormatType_32BGRA),
            (ScalerFormat::Argb, kCVPixelFormatType_32ARGB),
        ] {
            assert_eq!(format.fourcc().0, code, "{format:?}");
        }
    }

    #[test]
    fn a_record_that_does_not_read_is_bad_data() {
        let config = DecoderConfig {
            codec: Codec::H264,
            width: 64,
            height: 48,
            configuration: vec![1, 2],
        };
        assert!(matches!(
            output_format(&config),
            Err(BackendError::BadData(_))
        ));
    }

    #[test]
    fn depths_pick_nv12_or_p010_and_more_is_refused() {
        assert_eq!(biplanar(8, true).ok(), Some(PixelFormat::Nv12FullRange));
        assert_eq!(biplanar(10, false).ok(), Some(PixelFormat::P010VideoRange));
        assert!(matches!(
            biplanar(12, false),
            Err(BackendError::Unsupported(_))
        ));
        assert!(matches!(check_chroma(2), Err(BackendError::Unsupported(_))));
    }
}
