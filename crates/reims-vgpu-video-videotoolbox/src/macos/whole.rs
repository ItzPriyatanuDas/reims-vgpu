//! VideoToolbox as the video device takes a backend: core's [`Backend`],
//! with the host's hardware decoders and encoders and its motion
//! estimation, and no JPEG decoder.

use std::sync::Arc;

use reims_vgpu_video_core::backend::{Backend, DecoderBackend};
use reims_vgpu_video_core::encoder_backend::EncoderBackend;
use reims_vgpu_video_core::motion::MotionEstimationBackend;
use reims_vgpu_video_core::runtime::Runtime;

use super::decoder::VideoToolboxBackend;
use super::encoder::VideoToolboxEncoderBackend;
use super::motion::VideoToolboxMotionBackend;

/// What the host's log calls this backend.
pub const NAME: &str = "VideoToolbox";

/// The whole of VideoToolbox on this host: its hardware decoders and
/// encoders where it lists any, and its motion estimation where it has a
/// processor.
/// What each part offers is asked of VideoToolbox now, once, but the
/// encoders: they are listed whatever their state, and read once
/// VideoToolbox answers for them (`VideoToolboxEncoderBackend`). There is no
/// JPEG decoder: the guest is told the host has no JPEG engine, and
/// decodes JPEG itself. Its encoders bound their waits on `runtime`'s clock.
pub fn backend(runtime: Arc<dyn Runtime>) -> Backend {
    let decoders = VideoToolboxBackend::new();
    let encoders = VideoToolboxEncoderBackend::new(runtime);
    let motion = match VideoToolboxMotionBackend::new() {
        Ok(motion) => Some(Arc::new(motion) as Arc<dyn MotionEstimationBackend>),
        Err(why) => {
            log::info!("VideoToolbox: no motion estimation: {why}");
            None
        }
    };
    Backend {
        name: NAME.to_owned(),
        decoders: (!decoders.capabilities().is_empty())
            .then(|| Arc::new(decoders) as Arc<dyn DecoderBackend>),
        encoders: (!encoders.encoders().is_empty())
            .then(|| Arc::new(encoders) as Arc<dyn EncoderBackend>),
        motion,
        jpeg: None,
    }
}

#[cfg(test)]
mod tests {
    use reims_vgpu_video_core::runtime::StdRuntime;

    use super::*;

    #[test]
    fn the_whole_backend_has_every_part_but_jpeg() {
        let whole = || backend(StdRuntime::shared());
        let backend = super::super::tests::when_encoders_are_free(whole, |backend| {
            backend.encoders.is_some()
        });
        assert!(backend.decoders.is_some(), "{}", backend.summary());
        assert!(backend.encoders.is_some(), "{}", backend.summary());
        assert!(backend.jpeg.is_none());
        let summary = backend.summary();
        assert!(
            summary.starts_with("VideoToolbox: decodes H264"),
            "{summary}"
        );
    }
}
