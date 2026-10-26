//! The host session's encoder backend on VA-API: core's `EncoderBackend`
//! over [`super::VideoEncoder`], listing the encoders the driver reports,
//! beside [`super::super::VaapiBackend`]'s decoders.
//!
//! What the session drives of each encoder, and the choices made where the
//! guest sets nothing, are [`crate::encode::session`]'s.

use std::path::Path;
use std::sync::Arc;

use reims_vgpu_video_core::backend::{BackendError, Codec, PixelFormat};
use reims_vgpu_video_core::encoder_backend::{
    BackendEncoder, EncoderBackend, EncoderCapabilities, EncoderConfig,
};
use reims_vgpu_video_core::runtime::Runtime;

use super::super::display::{Display, Error};
use super::accelerator::VaAccelerator;
use super::{EncoderSupport, VideoEncoder, encoder_support};
use crate::encode::encoder::Encoder;
use crate::encode::session::{BackendFailure, SessionEncoder};
use crate::encode::{self, Profile};

impl BackendFailure for Error {
    /// What the driver lacks is unsupported, planes that do not hold the
    /// picture are bad data, and the rest is the driver's malfunction.
    fn into_backend(self) -> BackendError {
        let text = self.to_string();
        match self {
            Self::Unsupported(_) => BackendError::Unsupported(text),
            Self::PlaneTooSmall(_) => BackendError::BadData(text),
            _ => BackendError::Malfunction(text),
        }
    }
}

/// The encoders on one VA display.
pub struct VaapiEncoderBackend {
    display: Display,
    profiles: Vec<Profile>,
    listed: Vec<EncoderCapabilities>,
}

impl VaapiEncoderBackend {
    /// A backend on the render node at `path`, such as
    /// `/dev/dri/renderD128`, on `runtime`.
    pub fn open(path: impl AsRef<Path>, runtime: Arc<dyn Runtime>) -> Result<Self, Error> {
        Ok(Self::new(Display::open(path, runtime)?))
    }

    /// A backend on `display`. What it encodes is read from the driver now,
    /// once.
    pub fn new(display: Display) -> Self {
        let support = encoder_support(&display);
        Self {
            profiles: support.iter().map(|s| s.profile).collect(),
            listed: listed(&support),
            display,
        }
    }
}

/// The encoder list: per codec, the profiles the driver encodes, the
/// largest picture any of them takes, the source formats (8-bit first, as
/// Apple's AVC encoder lists `420v` and `420f` first; 10-bit too where HEVC
/// Main 10 is there), and whether B pictures are.
fn listed(support: &[EncoderSupport]) -> Vec<EncoderCapabilities> {
    [
        (Codec::H264, encode::Codec::H264),
        (Codec::Hevc, encode::Codec::Hevc),
    ]
    .into_iter()
    .filter_map(|(codec, ours)| {
        let of: Vec<&EncoderSupport> = support
            .iter()
            .filter(|s| s.profile.codec() == ours)
            .collect();
        if of.is_empty() {
            return None;
        }
        let mut source_formats = vec![PixelFormat::Nv12VideoRange, PixelFormat::Nv12FullRange];
        if of.iter().any(|s| s.profile.bit_depth() > 8) {
            source_formats.extend([PixelFormat::P010VideoRange, PixelFormat::P010FullRange]);
        }
        Some(EncoderCapabilities {
            codec,
            profiles: of
                .iter()
                .map(|s| u32::from(s.profile.profile_idc()))
                .collect(),
            max_width: of.iter().map(|s| s.max_width).max().unwrap_or(0),
            max_height: of.iter().map(|s| s.max_height).max().unwrap_or(0),
            source_formats,
            frame_reordering: of
                .iter()
                .any(|s| s.profile.has_b_slices() && s.capabilities.max_references().1 > 0),
        })
    })
    .collect()
}

impl EncoderBackend for VaapiEncoderBackend {
    fn encoders(&self) -> Vec<EncoderCapabilities> {
        self.listed.clone()
    }

    fn open(&self, config: &EncoderConfig) -> Result<Box<dyn BackendEncoder>, BackendError> {
        if !self
            .listed
            .iter()
            .any(|listed| listed.codec == config.codec)
        {
            return Err(BackendError::Unsupported(format!(
                "the driver encodes no {:?}",
                config.codec
            )));
        }
        let display = self.display.clone_ref();
        let encoder: SessionEncoder<Encoder<VaAccelerator>> = SessionEncoder::new(
            config,
            self.profiles.clone(),
            Box::new(move |settings| {
                VideoEncoder::new(&display, settings.clone())
                    .map(|encoder| encoder.inner)
                    .map_err(BackendFailure::into_backend)
            }),
        )?;
        Ok(Box::new(encoder))
    }
}
