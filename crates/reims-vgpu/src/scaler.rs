//! [`ScalerDevice`]: the host side of the paravirtualized M2 scaler device that a macOS guest's
//! AppleM2ScalerParavirtDriver drives (virtio-PCI `106b:1a0b`), made from a
//! [`ScalerDeviceConfig`]: reims-vgpu-video-core's scaler device, over this library's IOSurface
//! mapper and a backend that transforms on the host's GPU.
//!
//! The device has no transport of its own. Its host (QEMU's `apple-m2scaler-pci`) owns the one
//! virtqueue: it hands each element's readable bytes to [`ScalerDevice::process`], with the size
//! of its writable part and the guest's memory, and does what the [`ScalerOutcome`] says: write
//! the reply, set DEVICE_NEEDS_RESET, or end the VM, as Apple's host does each.
//!
//! - **Lifecycle**, as Apple's host runs it: [`ScalerDevice::start`], at the guest's DRIVER_OK,
//!   makes the backend's accelerator, and [`ScalerDevice::reset`] drops it. A start that cannot
//!   make one is a needs-reset.
//! - **Requests.** Type 1 writes the capabilities into the guest's memory; type 2 transforms one
//!   guest IOSurface into another, the two named by mapping ID and held for the transform through
//!   the IOSurface device the config names, as the video device holds its surfaces. The source is
//!   read through [`ApiPlatform::lend_surface_bytes_for_reading`], the destination written
//!   through [`ApiPlatform::lend_surface_bytes`].
//! - **The backend** is our own kernel on the GPU the library draws with ([`gpu`]): Metal on
//!   macOS, the Vulkan platform's runtime elsewhere, on the device the library's GPU device runs
//!   on, the loader's pin included, whichever GPU that is. It computes core's reference transform
//!   (`reims_vgpu_video_core::scaler::reference`), reading and writing the guest's surfaces in
//!   place where the host lends their memory to devices.
//! - **A host without that GPU** has the device all the same, so that a VM given it always
//!   starts: [`ScalerDevice::scales`] says false, and the host refuses the guest's features,
//!   leaving FEATURES_OK unset. That is a host where Metal or the Vulkan runtime finds no device,
//!   or refuses the kernel. The guest's driver reads back every status bit it sets, so its start
//!   fails before DRIVER_OK: it writes FAILED and registers no service, and the guest's
//!   IOSurfaceAccelerator does without it, as in a VM without the device. A guest that went on
//!   would find the start a needs-reset, as Apple's host answers where it cannot make an
//!   accelerator, and every request before it a needs-reset.
//!
//! Every call runs as an entry ([`crate::entry`]).

use std::fmt;
use std::sync::{Arc, Once};

use reims_vgpu_video_core::runtime::Runtime;
use reims_vgpu_video_core::scaler::ScalerDevice as CoreScalerDevice;
use reims_vgpu_video_core::scaler::backend::{Accelerator, ScalerBackend};
pub use reims_vgpu_video_core::scaler::{GuestMemory, Outcome as ScalerOutcome};
use reims_vgpu_video_core::surfaces::Surfaces;

use crate::DefaultPlatform;
use crate::entry;
use crate::iosurface::IoSurfaceDevice;
use crate::platform::ApiPlatform;
use crate::video::MapperSurfaces;
use crate::video::runtime::PlatformRuntime;

mod gpu;

use gpu::{Gpu as _, GpuScalerBackend};

/// A scaler device's config.
pub struct ScalerDeviceConfig<P: ApiPlatform = DefaultPlatform> {
    /// The IOSurface device whose mapping IDs the guest's transforms name, held unretained;
    /// `None` is the process's latest at each lookup.
    pub iosurface_device: Option<IoSurfaceDevice<P>>,
}

impl<P: ApiPlatform> Default for ScalerDeviceConfig<P> {
    fn default() -> Self {
        ScalerDeviceConfig {
            iosurface_device: None,
        }
    }
}

impl<P: ApiPlatform> fmt::Debug for ScalerDeviceConfig<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScalerDeviceConfig")
            .field("iosurface_device", &self.iosurface_device)
            .finish()
    }
}

/// The host side of one M2 scaler device (module doc).
pub struct ScalerDevice<P: ApiPlatform = DefaultPlatform> {
    device: CoreScalerDevice,
    /// The guest's surfaces, kept for the mapper's description.
    surfaces: Arc<MapperSurfaces<P>>,
    /// Why the host has no GPU for the device; `None` where it has.
    unscaled: Option<String>,
}

impl<P: ApiPlatform> fmt::Debug for ScalerDevice<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScalerDevice")
            .field("mapper", &self.surfaces.names_its_mapper())
            .field("unscaled", &self.unscaled)
            .finish_non_exhaustive()
    }
}

impl ScalerDevice {
    /// A new scaler device on the process's platform ([`ScalerDevice::with_platform`]).
    pub fn new(config: ScalerDeviceConfig) -> ScalerDevice {
        ScalerDevice::with_platform(DefaultPlatform::process(), config)
    }
}

impl<P: ApiPlatform> ScalerDevice<P> {
    /// A new scaler device on `platform`, transforming on the host's GPU (module doc), with no
    /// accelerator until [`Self::start`]. Made on every host: where there is no GPU it is one
    /// that does not scale ([`Self::scales`]).
    pub fn with_platform(platform: Arc<P>, config: ScalerDeviceConfig<P>) -> ScalerDevice<P> {
        let runtime = PlatformRuntime::of(&platform);
        ScalerDevice::on_host_gpu(platform, &runtime, config)
    }

    /// [`Self::with_platform`], its transforms' waits for the GPU on `runtime`'s clock: the
    /// platform's, or, in a test of the host's GPU, std's, since such a wait ends only on a clock
    /// that runs.
    pub(crate) fn on_host_gpu(
        platform: Arc<P>,
        runtime: &Arc<dyn Runtime>,
        config: ScalerDeviceConfig<P>,
    ) -> ScalerDevice<P> {
        entry::run(|_| match gpu_backend(runtime) {
            Ok(backend) => ScalerDevice::make(platform, config, backend, None),
            Err(why) => ScalerDevice::unscaled(platform, config, why),
        })
    }

    /// A new scaler device on `platform` whose transforms run on `backend`: for a host that
    /// brings its own, and for the tests.
    pub fn with_backend(
        platform: Arc<P>,
        config: ScalerDeviceConfig<P>,
        backend: Arc<dyn ScalerBackend>,
    ) -> ScalerDevice<P> {
        entry::run(|_| ScalerDevice::make(platform, config, backend, None))
    }

    /// A new scaler device on `platform` with no GPU to transform on, `why` in words: one that
    /// does not scale ([`Self::scales`]), for a host that has none, and for the tests.
    pub fn without_gpu(
        platform: Arc<P>,
        config: ScalerDeviceConfig<P>,
        why: String,
    ) -> ScalerDevice<P> {
        entry::run(|_| ScalerDevice::unscaled(platform, config, why))
    }

    /// Whether the device transforms: false where the host has no GPU for it, and the host
    /// refuses the guest's features (module doc).
    pub fn scales(&self) -> bool {
        self.unscaled.is_none()
    }

    fn unscaled(platform: Arc<P>, config: ScalerDeviceConfig<P>, why: String) -> ScalerDevice<P> {
        log::info!(
            "scaler device: {why}: it refuses the guest's features, so the guest's driver does \
             without it"
        );
        let backend = Arc::new(NoGpu(why.clone()));
        ScalerDevice::make(platform, config, backend, Some(why))
    }

    fn make(
        platform: Arc<P>,
        config: ScalerDeviceConfig<P>,
        backend: Arc<dyn ScalerBackend>,
        unscaled: Option<String>,
    ) -> ScalerDevice<P> {
        let surfaces = Arc::new(MapperSurfaces::new(
            "scaler device",
            platform,
            config.iosurface_device.as_ref(),
        ));
        ScalerDevice {
            device: CoreScalerDevice::new(Arc::clone(&surfaces) as Arc<dyn Surfaces>, backend),
            surfaces,
            unscaled,
        }
    }

    /// The guest's DRIVER_OK: a new accelerator, replacing the one there was. `Err` with
    /// Apple's reason where the backend cannot make one: the host sets DEVICE_NEEDS_RESET.
    pub fn start(&self) -> Result<(), &'static str> {
        entry::run(|_| {
            let started = self.device.start();
            match started {
                Ok(()) => log::info!("scaler device started"),
                Err(why) => log::warn!("scaler device did not start: {why}"),
            }
            started
        })
    }

    /// One element of the queue: `request` its readable bytes, `writable` the size of its
    /// writable part, `memory` the guest's, through which type 1 writes the capabilities and
    /// type 2 gathers and scatters its side buffers. What the host does with the element
    /// ([`ScalerOutcome`]). May block on the IOSurface device's thread and on the GPU.
    pub fn process(
        &self,
        request: &[u8],
        writable: usize,
        memory: &dyn GuestMemory,
    ) -> ScalerOutcome {
        entry::run(|_| {
            let outcome = self.device.process(request, writable, memory);
            let word = |bytes: &[u8], at: usize| {
                bytes.get(at..at + 4).map_or_else(
                    || "none".to_owned(),
                    |b| format!("{:#x}", u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                )
            };
            let (kind, n) = (word(request, 0), request.len());
            match &outcome {
                ScalerOutcome::Reply(reply) => log::debug!(
                    "scaler request type {kind}, {n} bytes: status {}",
                    word(reply, 4)
                ),
                ScalerOutcome::NeedsReset(why) => {
                    log::warn!("scaler request type {kind}, {n} bytes: needs reset: {why}");
                }
                ScalerOutcome::EndVm(why) => {
                    log::error!("scaler request type {kind}, {n} bytes: ends the VM: {why}");
                }
            }
            outcome
        })
    }

    /// A reset or a stop: the accelerator goes.
    pub fn reset(&self) {
        entry::run(|_| {
            self.device.reset();
            log::info!("scaler device reset");
        });
    }
}

/// The backend on the process's GPU, opened at its first use: Metal on macOS, the Vulkan
/// platform's runtime elsewhere. `Err` with why where there is none, or it refuses the kernel.
/// The first open of a process starts the making of the kernels' pipelines ahead of the guest's
/// first transform, on a thread of `runtime`'s, which nothing waits for ([`gpu::make_ahead`]).
fn gpu_backend(runtime: &Arc<dyn Runtime>) -> Result<Arc<dyn ScalerBackend>, String> {
    #[cfg(target_os = "macos")]
    let gpu = gpu::metal::MetalGpu::process()?;
    #[cfg(not(target_os = "macos"))]
    let gpu = gpu::vulkan::VulkanGpu::process(runtime)?;
    log::info!("scaler device: transforms on {}", gpu.name());
    static AHEAD: Once = Once::new();
    AHEAD.call_once(|| {
        if let Err(why) = gpu::make_ahead(&gpu, runtime) {
            log::warn!("scaler: {why}: each kernel is made at its first transform");
        }
    });
    Ok(Arc::new(GpuScalerBackend::new(gpu, Arc::clone(runtime))))
}

/// The backend of a device without a GPU: it makes no accelerator, so its start is a
/// needs-reset, as Apple's host answers where it cannot make one.
struct NoGpu(String);

impl ScalerBackend for NoGpu {
    fn create_accelerator(&self) -> Result<Box<dyn Accelerator>, String> {
        Err(self.0.clone())
    }
}

#[cfg(test)]
mod tests;
