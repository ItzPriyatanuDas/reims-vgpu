//! The backend's AV1 half: what the driver decodes of AV1, and
//! [`Av1Decoder`] as each `av01` session the guest starts drives it.

use reims_vgpu_video_core::backend::{
    BackendError, Codec, CodecCapabilities, DecoderConfig, FrameDecoded, PixelFormat,
    ProfileSupport,
};

use super::super::av1::{AV1_MAIN, Av1Decoder};
use super::super::caps::Entrypoint;
use super::super::display::{Display, Error};
use super::super::ffi;
use super::super::frame::Frame;
use super::{StreamDecoder, backend_error, max_size, pixel_format};
use crate::av1::{av1c::ConfigurationRecord, sequence::SequenceHeader};

/// What the driver decodes of AV1: Main (`seq_profile` 0) where it has
/// `VAProfileAV1Profile0` with both 8-bit and 10-bit 4:2:0 surfaces, since
/// a Main decoder decodes both and the host lists both depths for it; and
/// the largest picture it takes. None otherwise.
pub(super) fn capabilities(display: &Display) -> Option<CodecCapabilities> {
    if !display.supports(AV1_MAIN, Entrypoint::VLD) {
        return None;
    }
    let formats = display
        .config_attributes(AV1_MAIN, Entrypoint::VLD, &[ffi::VAConfigAttribRTFormat])
        .ok()?
        .first()
        .copied()
        .flatten()?;
    let main = ffi::VA_RT_FORMAT_YUV420 | ffi::VA_RT_FORMAT_YUV420_10;
    if formats & main != main {
        return None;
    }
    let (max_width, max_height) = max_size(display, AV1_MAIN);
    Some(CodecCapabilities {
        codec: Codec::Av1,
        // VA-API reports no level: the host session lists Apple's.
        profiles: vec![ProfileSupport {
            profile: 0,
            max_level: None,
        }],
        max_width,
        max_height,
    })
}

/// AV1's frames come in the depth and range of the record's sequence
/// header, which a record the session goes on with keeps. AV1 holds no
/// frame back: each temporal unit shows its own, so a flush has none.
impl StreamDecoder for Av1Decoder {
    const CODEC: Codec = Codec::Av1;

    fn open(
        display: &Display,
        config: &DecoderConfig,
    ) -> Result<(Self, PixelFormat), BackendError> {
        if config.configuration.is_empty() {
            // The pixel format the guest allocates is asked for before the
            // first sample, and comes from the record.
            return Err(BackendError::Unsupported(
                "an av01 description without its av1C record".into(),
            ));
        }
        let decoder = Av1Decoder::new(display, &config.configuration).map_err(backend_error)?;
        Ok((decoder, av1c_format(&config.configuration)))
    }

    fn accepts(display: &Display, config: &DecoderConfig, format: PixelFormat) -> bool {
        av1c_format(&config.configuration) == format
            && Av1Decoder::new(display, &config.configuration).is_ok()
    }

    fn decode(&mut self, sample: &[u8], id: u64) -> Result<Vec<Frame>, Error> {
        Av1Decoder::decode(self, sample, id)
    }

    /// Each frame the temporal unit carried, for the session to lend the
    /// guest's buffers by, as Apple's AV1 decoder lends them.
    fn frames_decoded(&mut self) -> Vec<FrameDecoded> {
        self.steps().iter().map(FrameDecoded::from).collect()
    }
}

/// The pixel format the `av1C` record's stream decodes into: P010 for 10
/// bits, NV12 for 8, in the range its sequence header says; video range
/// when the record carries no sequence header that reads.
fn av1c_format(av1c: &[u8]) -> PixelFormat {
    let Ok(record) = ConfigurationRecord::parse(av1c) else {
        return pixel_format(8, false);
    };
    let full_range = record
        .sequence_header_obu()
        .ok()
        .flatten()
        .and_then(|obu| SequenceHeader::parse(obu).ok())
        .is_some_and(|sequence| sequence.color.color_range);
    pixel_format(u32::from(record.bit_depth()), full_range)
}
