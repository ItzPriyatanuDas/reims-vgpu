//! NVIDIA as the devices take a backend: core's [`Backend`], with NVDEC's
//! decoders, NVENC's encoders, NVENC's motion estimation and nvJPEG's
//! hardware JPEG decoder on one GPU.

use std::sync::Arc;

use reims_vgpu_video_core::backend::{Backend, DecoderBackend};
use reims_vgpu_video_core::encoder_backend::EncoderBackend;
use reims_vgpu_video_core::jpeg::JpegDecoder;
use reims_vgpu_video_core::motion::MotionEstimationBackend;
use reims_vgpu_video_core::runtime::Runtime;

use crate::cuda::{Gpu, GpuSelector};
use crate::decode::NvidiaDecoders;
use crate::encode::NvidiaEncoders;
use crate::error::Error;
use crate::jpeg::NvidiaJpegDecoder;
use crate::motion::NvidiaMotion;

/// What the host's log calls this backend.
pub const NAME: &str = "NVIDIA";

/// The whole of NVIDIA's backend on the GPU `gpu` names: a CUDA ordinal, a
/// PCI bus ID, a DRM node such as `/dev/dri/renderD128`, or nothing for the
/// first. What each part offers is asked of the driver now, once. Only a
/// host without the driver, or a GPU that does not open, fails. The threads
/// it starts, and the clock its waits on the GPU are bounded by, are
/// `runtime`'s (`crate::wait`).
pub fn open_backend(gpu: &str, runtime: Arc<dyn Runtime>) -> Result<Backend, Error> {
    crate::wait::use_runtime(&runtime);
    let gpu = Arc::new(Gpu::open(&GpuSelector::parse(gpu))?);
    let decoders = NvidiaDecoders::new(Arc::clone(&gpu), &runtime);
    let encoders = match NvidiaEncoders::new(Arc::clone(&gpu)) {
        Ok(encoders) => Some(encoders),
        Err(error) => {
            log::warn!(
                "NVIDIA {}: NVENC did not probe ({error}): no encoders",
                gpu.info().pci_bus_id
            );
            None
        }
    };
    let motion = match NvidiaMotion::new(Arc::clone(&gpu)) {
        Ok(motion) => Some(Arc::new(motion) as Arc<dyn MotionEstimationBackend>),
        Err(error) => {
            log::info!(
                "NVIDIA {}: no motion estimation ({error})",
                gpu.info().pci_bus_id
            );
            None
        }
    };
    // libnvjpeg is a toolkit library, which a host may not have: without it,
    // or without the GPU's JPEG engine, the guest is told the host has no
    // JPEG engine, as Apple's host without JPEG hardware tells it.
    let jpeg = match NvidiaJpegDecoder::new(Arc::clone(&gpu)) {
        Ok(decoder) => Some(Arc::new(decoder) as Arc<dyn JpegDecoder>),
        Err(error) => {
            log::info!("NVIDIA {}: no JPEG engine ({error})", gpu.info().pci_bus_id);
            None
        }
    };
    Ok(Backend {
        name: format!("{NAME} ({}, {})", gpu.info().name, gpu.info().pci_bus_id),
        decoders: (!decoders.capabilities().is_empty())
            .then(|| Arc::new(decoders) as Arc<dyn DecoderBackend>),
        encoders: encoders
            .filter(|encoders| !encoders.encoders().is_empty())
            .map(|encoders| Arc::new(encoders) as Arc<dyn EncoderBackend>),
        motion,
        jpeg,
    })
}
