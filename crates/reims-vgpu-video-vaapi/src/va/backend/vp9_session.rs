//! The backend's VP9 half: what the driver decodes of VP9, and
//! [`Vp9Decoder`] as each `vp09` session the guest starts drives it.

use reims_vgpu_video_core::backend::{
    BackendError, Codec, CodecCapabilities, DecoderConfig, FrameDecoded, PixelFormat,
    ProfileSupport,
};

use super::super::caps::{Entrypoint, Profile};
use super::super::display::{Display, Error};
use super::super::ffi;
use super::super::frame::Frame;
use super::super::vp9::{VP9_PROFILE0, VP9_PROFILE2, Vp9Decoder};
use super::{StreamDecoder, backend_error, max_size, pixel_format};
use crate::vp9::vpcc::ConfigurationRecord;

/// VP9's profiles as the bitstream numbers them, each with its VA profile
/// and the render target format its 4:2:0 streams decode into.
const VP9_PROFILES: [(u32, Profile, u32); 2] = [
    (0, VP9_PROFILE0, ffi::VA_RT_FORMAT_YUV420),
    (2, VP9_PROFILE2, ffi::VA_RT_FORMAT_YUV420_10),
];

/// The VA profiles VP9 decodes on.
pub(super) const VA_PROFILES: [Profile; 2] = [VP9_PROFILE0, VP9_PROFILE2];

/// What the driver decodes of VP9: Profile 0 where it has
/// `VAProfileVP9Profile0` with 8-bit 4:2:0 surfaces, Profile 2 where it has
/// `VAProfileVP9Profile2` with 10-bit ones, and the largest picture those
/// take. None when it has neither. The guest is told none of the profiles,
/// as Apple's host tells it none: they say only whether the decoder is
/// listed.
pub(super) fn capabilities(display: &Display) -> Option<CodecCapabilities> {
    let mut profiles = Vec::new();
    let (mut max_width, mut max_height) = (0, 0);
    for (number, profile, format) in VP9_PROFILES {
        if !display.supports(profile, Entrypoint::VLD) {
            continue;
        }
        let formats = display
            .config_attributes(profile, Entrypoint::VLD, &[ffi::VAConfigAttribRTFormat])
            .ok()
            .and_then(|values| values.first().copied().flatten())
            .unwrap_or(0);
        if formats & format == 0 {
            continue;
        }
        // VA-API reports no level, and the host lists none for VP9.
        profiles.push(ProfileSupport {
            profile: number,
            max_level: None,
        });
        let (width, height) = max_size(display, profile);
        max_width = max_width.max(width);
        max_height = max_height.max(height);
    }
    (!profiles.is_empty()).then_some(CodecCapabilities {
        codec: Codec::Vp9,
        profiles,
        max_width,
        max_height,
    })
}

/// One VP9 decode session.
/// VP9's frames come in the depth and range of the `vpcC` record, which a
/// record the session goes on with keeps. VP9 holds no frame back.
impl StreamDecoder for Vp9Decoder {
    const CODEC: Codec = Codec::Vp9;

    fn open(
        display: &Display,
        config: &DecoderConfig,
    ) -> Result<(Self, PixelFormat), BackendError> {
        if config.configuration.is_empty() {
            return Err(BackendError::Unsupported(
                "a vp09 description without its vpcC record".into(),
            ));
        }
        let decoder = Vp9Decoder::new(display, &config.configuration).map_err(backend_error)?;
        let format = vpcc_format(&config.configuration)
            .ok_or_else(|| BackendError::BadData("a vpcC record that does not read".into()))?;
        Ok((decoder, format))
    }

    fn accepts(display: &Display, config: &DecoderConfig, format: PixelFormat) -> bool {
        vpcc_format(&config.configuration) == Some(format)
            && Vp9Decoder::new(display, &config.configuration).is_ok()
    }

    fn decode(&mut self, sample: &[u8], id: u64) -> Result<Vec<Frame>, Error> {
        Vp9Decoder::decode(self, sample, id)
    }

    /// Each frame the sample carried, those refused included, for the
    /// session to lend the guest's buffers by, as Apple's VP9 decoder lends
    /// them; read once, so that a decode that fails before this decoder
    /// says none.
    fn frames_decoded(&mut self) -> Vec<FrameDecoded> {
        self.take_steps().iter().map(FrameDecoded::from).collect()
    }
}

/// The pixel format the `vpcC` record says, None where it does not read.
fn vpcc_format(vpcc: &[u8]) -> Option<PixelFormat> {
    let record = ConfigurationRecord::parse(vpcc).ok()?;
    Some(pixel_format(u32::from(record.bit_depth), record.full_range))
}
