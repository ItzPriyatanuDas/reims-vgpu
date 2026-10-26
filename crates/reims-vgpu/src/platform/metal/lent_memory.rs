//! A Metal surface's memory as a video device's decoders lend it to a device
//! ([`ApiPlatform::surface_device_memory`](crate::platform::ApiPlatform::surface_device_memory)):
//! the Metal crate's lend of its IOSurface (`reims_vgpu_metal::os::device_lend`), and an identity
//! attached to the surface (`reims_vgpu_metal::os::surface_bytes::attachment`). The identity is
//! one for every object over the surface, and goes when a surface made later takes its
//! `IOSurfaceID`, so the same pages mapped again are another surface with another identity, and no
//! import of the one before is ever written again.
//!
//! The IOSurface is borrowed for the lend's life, which is the holder's hold: anything a device
//! makes of it, a pixel buffer over it among them, is released before its write returns, as a
//! reference of it is a hold the guest sees (the mapper's unmap answers Busy while one lives).

use reims_vgpu_metal::os::device_lend::{DeviceLend, lend_to_devices};
use reims_vgpu_metal::os::iosurface::MetalSurface;
use reims_vgpu_metal::os::surface_bytes::attachment;
use reims_vgpu_video_core::device_memory::{
    DeviceMemory, LentIoSurface, MemoryIdentity, MemoryLife,
};
use reims_vgpu_video_core::surfaces::SurfaceError;

/// A surface's memory lent to a device (module docs).
pub(super) struct LentMemory {
    identity: MemoryIdentity,
    lend: DeviceLend,
}

/// `surface`'s memory lent to a device; `Err` with why where it is not lent: a protected surface,
/// and one the guest made read-only for its clients.
pub(super) fn lent_memory(surface: &MetalSurface) -> Result<LentMemory, &'static str> {
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

    /// The surface's own IOSurface, and whether a device may write it, asked again now
    /// (`DeviceLend::writable`).
    fn io_surface(&self) -> Result<LentIoSurface<'_>, SurfaceError> {
        // SAFETY: the lend holds a reference of the IOSurface for as long as it, and so this
        // borrow, lives.
        Ok(unsafe { LentIoSurface::new(self.lend.io_surface(), self.lend.writable()) })
    }
}
