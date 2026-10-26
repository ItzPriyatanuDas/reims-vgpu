//! VA-API as the video device takes a backend: core's [`Backend`], with
//! the decoders, encoders and JPEG decoder of one render node on one
//! display.

use std::path::Path;
use std::sync::Arc;

use reims_vgpu_video_core::backend::{Backend, DecoderBackend};
use reims_vgpu_video_core::encoder_backend::EncoderBackend;
use reims_vgpu_video_core::jpeg::JpegDecoder;
use reims_vgpu_video_core::runtime::Runtime;

use super::backend::VaapiBackend;
use super::display::{Display, Error};
use super::encoder::VaapiEncoderBackend;
use super::jpeg::VaapiJpegDecoder;

/// What the host's log calls this backend.
pub const NAME: &str = "VA-API";

/// The whole of VA-API on the render node at `path`, such as
/// `/dev/dri/renderD128`, on one display: its decoders, its encoders where
/// the driver lists any, and its JPEG decoder where the driver has one.
/// What each part offers is read from the driver now, once. Only a display
/// that does not open fails. The threads it starts and the clock it times
/// by are `runtime`'s.
pub fn open_backend(path: impl AsRef<Path>, runtime: Arc<dyn Runtime>) -> Result<Backend, Error> {
    let display = Display::open(path, runtime)?;
    Ok(backend_on(&display))
}

/// The same, on a display already open.
pub fn backend_on(display: &Display) -> Backend {
    let decoders = VaapiBackend::new(display.clone_ref());
    let encoders = VaapiEncoderBackend::new(display.clone_ref());
    let jpeg = match VaapiJpegDecoder::new(display.clone_ref()) {
        Ok(jpeg) => {
            let (width, height) = jpeg.max_size();
            log::info!(
                "VA-API at {}: JPEG {:?} up to {width}x{height}",
                display.path(),
                jpeg.subsamplings()
            );
            Some(Arc::new(jpeg) as Arc<dyn JpegDecoder>)
        }
        Err(Error::Unsupported(why)) => {
            log::info!("VA-API at {}: no JPEG decoder: {why}", display.path());
            None
        }
        Err(error) => {
            log::warn!(
                "VA-API at {}: its JPEG decoder did not probe ({error}): no JPEG",
                display.path()
            );
            None
        }
    };
    Backend {
        name: format!("{NAME} ({})", display.vendor()),
        decoders: (!decoders.capabilities().is_empty())
            .then(|| Arc::new(decoders) as Arc<dyn DecoderBackend>),
        encoders: (!encoders.encoders().is_empty())
            .then(|| Arc::new(encoders) as Arc<dyn EncoderBackend>),
        motion: None,
        jpeg,
    }
}
