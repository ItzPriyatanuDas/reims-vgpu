//! A Linux surface's memory as a video device's decoders lend it to a device
//! ([`ApiPlatform::surface_device_memory`](crate::platform::ApiPlatform::surface_device_memory)):
//! the Vulkan crate's lend of its bytes (`reims_vgpu_vulkan::os::lend_to_devices`), and an
//! identity attached to the surface for as long as it lives (`reims_vgpu_vulkan::os::attachment`).
//! The identity goes with the surface's last object, when the guest has unmapped it and nothing
//! holds it, and a device's imports of its memory with it (reims-vgpu-video-core's
//! `DeviceImports`); the same pages mapped again are another surface, with another identity, so
//! no import of the one before is ever written again.

use reims_vgpu_core::num::ToU64 as _;
use reims_vgpu_video_core::device_memory::{
    DeviceMemory, DmaBufError, HostView, MemoryIdentity, MemoryLife, SurfaceDmaBuf,
};
use reims_vgpu_video_core::surfaces::SurfaceError;
use reims_vgpu_vulkan::os::{DeviceLend, LendRefusal, OsSurface, attachment, lend_to_devices};

/// A surface's memory lent to a device (module docs).
pub(super) struct LentMemory {
    identity: MemoryIdentity,
    lend: DeviceLend,
}

/// `surface`'s memory lent to a device; `Err` with why where it is not lent.
pub(super) fn lent_memory(surface: &OsSurface) -> Result<LentMemory, &'static str> {
    let lend = lend_to_devices(surface)?;
    let identity = attachment(surface, MemoryLife::new, MemoryLife::identity)
        .ok_or("a surface the host attached another value to")?;
    Ok(LentMemory { identity, lend })
}

impl DeviceMemory for LentMemory {
    fn identity(&self) -> MemoryIdentity {
        self.identity.clone()
    }

    fn length(&self) -> usize {
        usize::try_from(self.lend.length()).unwrap_or(usize::MAX)
    }

    fn dma_buf_of(&self, ranges: &[(usize, usize)]) -> Result<SurfaceDmaBuf, DmaBufError> {
        let ranges: Vec<(u64, u64)> = (ranges.iter())
            .map(|&(at, bytes)| (at.to_u64(), bytes.to_u64()))
            .collect();
        match self.lend.dma_buf_of(&ranges) {
            Ok(made) => Ok(SurfaceDmaBuf {
                fd: made.fd,
                size: made.size,
                offsets: made.offsets,
            }),
            Err(LendRefusal::TooLarge {
                pieces,
                bytes,
                most_pieces,
                most_bytes,
                why,
            }) => Err(DmaBufError::TooLarge {
                pieces,
                bytes,
                most_pieces,
                most_bytes,
                why,
            }),
            Err(LendRefusal::Refused(why)) => Err(DmaBufError::Refused(why)),
        }
    }

    fn host_view(&self) -> Result<HostView, SurfaceError> {
        let (first, size, offset) = self.lend.pages();
        let too_large = || SurfaceError::Unreachable("a view past the address space".to_owned());
        Ok(HostView {
            first,
            size: usize::try_from(size).map_err(|_| too_large())?,
            offset: usize::try_from(offset).map_err(|_| too_large())?,
            hold: self.lend.hold(),
        })
    }
}
