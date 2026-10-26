//! `MetalPlatform` as an `ApiPlatform`: the product's platform, on macOS.

use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLCreateSystemDefaultDevice, MTLOrigin, MTLRegion, MTLSize, MTLTexture};
use reims_vgpu_core::platform::Platform;
use reims_vgpu_metal::MetalPlatform;
use reims_vgpu_metal::msg;
use reims_vgpu_metal::os::surface_bytes;
use reims_vgpu_metal::platform::Obj;

use super::*;

mod lent_memory;

impl ApiPlatform for MetalPlatform {
    fn process() -> Arc<MetalPlatform> {
        crate::singleton::platform()
    }

    /// `MTLCreateSystemDefaultDevice()`, or the device of `MTLCopyAllDevices()` whose
    /// `registryID` matches (`Platform::open_device`). Each send runs in the message layer,
    /// which dies at the send if Metal raises.
    fn metal_device(&self, registry_id: Option<u64>) -> Option<<Self as Platform>::Device> {
        match registry_id {
            None => msg::get("MTLCreateSystemDefaultDevice()", || {
                MTLCreateSystemDefaultDevice()
            })
            .map(Obj::from_retained),
            Some(id) => self.open_device(id),
        }
    }

    /// `getBytes:bytesPerRow:fromRegion:mipmapLevel:` over the whole texture.
    fn read_pixels(
        &self,
        texture: &<Self as Platform>::Texture,
        dst: &mut [u8],
        bytes_per_row: usize,
    ) {
        let t: &ProtocolObject<dyn MTLTexture> = texture.get();
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: msg::get("width", || t.width()),
                height: msg::get("height", || t.height()),
                depth: 1,
            },
        };
        let Some(bytes) = std::ptr::NonNull::new(dst.as_mut_ptr().cast()) else {
            return;
        };
        msg::get("getBytes:bytesPerRow:fromRegion:mipmapLevel:", || {
            // SAFETY: the caller checked that `dst` holds `height` rows `bytes_per_row`
            // apart, each `width * 4` bytes (a 4-byte format); Metal writes only there.
            unsafe {
                t.getBytes_bytesPerRow_fromRegion_mipmapLevel(bytes, bytes_per_row, region, 0)
            }
        });
    }

    /// Each plane the surface was made with, from IOSurface's per-plane getters
    /// (`reims_vgpu_metal::os::surface_bytes::plane_layout`).
    fn surface_planes(
        &self,
        surface: &<Self as Platform>::Surface,
    ) -> Vec<crate::video::SurfacePlane> {
        use reims_vgpu_core::platform::os::surface::Surface as _;
        (0..surface.plane_count())
            .map_while(|i| surface_bytes::plane_layout(surface, i))
            .map(|p| crate::video::SurfacePlane {
                offset: p.offset,
                width: p.width,
                height: p.height,
                bytes_per_element: p.bytes_per_element,
                bytes_per_row: p.bytes_per_row,
            })
            .collect()
    }

    /// Through the surface's base address under `IOSurfaceLock`
    /// (`reims_vgpu_metal::os::surface_bytes`).
    fn lend_surface_bytes(
        &self,
        surface: &<Self as Platform>::Surface,
        write: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), &'static str> {
        surface_bytes::lend_surface_bytes(surface, write)
    }

    /// Through the surface's base address under a read-only `IOSurfaceLock`
    /// (`reims_vgpu_metal::os::surface_bytes`).
    fn lend_surface_bytes_for_reading(
        &self,
        surface: &<Self as Platform>::Surface,
        read: &mut dyn FnMut(&[u8]),
    ) -> Result<(), &'static str> {
        surface_bytes::lend_surface_bytes_for_reading(surface, read)
    }

    /// The surface's own IOSurface, which a device writes through VideoToolbox, with an identity
    /// attached to the surface (`platform/metal/lent_memory.rs`). Refused as `lend_surface_bytes`
    /// refuses a writer, and besides for any surface the guest made read-only for its clients,
    /// over its pages or its own memory (`reims_vgpu_metal::os::device_lend`).
    fn surface_device_memory(
        &self,
        surface: &<Self as Platform>::Surface,
    ) -> Result<Box<dyn DeviceMemory>, &'static str> {
        lent_memory::lent_memory(surface).map(|lent| Box::new(lent) as Box<dyn DeviceMemory>)
    }
}

#[cfg(test)]
mod tests {
    use reims_vgpu_core::platform::os::surface::{IoSurfaces as _, SurfacePlaneSpec, SurfaceSpec};
    use reims_vgpu_metal::os::iosurface::MetalSurfaces;

    use super::*;

    /// The video devices reach a surface's planes and bytes through the process's platform: each
    /// plane as it was made, a write lend's bytes in what a read lend sees
    #[test]
    fn a_surfaces_planes_and_bytes_reach_the_video_devices() {
        let plane = |offset, width, height, bytes_per_element| SurfacePlaneSpec {
            base: offset,
            offset,
            width,
            height,
            size: 64 * height,
            bytes_per_row: 64,
            bytes_per_element,
            element_width: 1,
            element_height: 1,
            ..SurfacePlaneSpec::default()
        };
        let spec = SurfaceSpec {
            planes: Some(vec![plane(0, 64, 32, 1), plane(4096, 32, 16, 2)]),
            pixel_format: u32::from_be_bytes(*b"420v"),
            alloc_size: 16384,
            width: 64,
            height: 32,
            bytes_per_element: 1,
            bytes_per_row: 64,
            element_width: 1,
            element_height: 1,
            ..SurfaceSpec::default()
        };
        let surface = MetalSurfaces::new().create(&spec).unwrap();
        let platform = MetalPlatform::process();
        let planes = platform.surface_planes(&surface);
        let offsets: Vec<_> = planes
            .iter()
            .map(|p| (p.offset, p.width, p.height))
            .collect();
        assert_eq!(offsets, [(0, 64, 32), (4096, 32, 16)]);
        assert_eq!(planes[1].bytes_per_element, 2);
        platform
            .lend_surface_bytes(&surface, &mut |bytes| bytes[4096] = 0x80)
            .unwrap();
        let mut seen = 0;
        platform
            .lend_surface_bytes_for_reading(&surface, &mut |bytes| seen = bytes[4096])
            .unwrap();
        assert_eq!(seen, 0x80);
    }

    /// A surface's memory is lent to a device as its own IOSurface, writable, with one identity
    /// for every lend of it; the lend's reference of the IOSurface goes with it. One the guest made
    /// read-only for its clients is lent to none. Both are of their own memory: no test here makes
    /// a surface over client memory
    #[test]
    fn a_surface_is_lent_to_a_device_as_its_own_iosurface() {
        use reims_vgpu_core::platform::Handle as _;
        use reims_vgpu_core::platform::os::surface::Surface as _;

        let spec = |client_direction| SurfaceSpec {
            pixel_format: u32::from_be_bytes(*b"BGRA"),
            alloc_size: 16384,
            width: 64,
            height: 64,
            bytes_per_element: 4,
            bytes_per_row: 256,
            element_width: 1,
            element_height: 1,
            client_direction,
            ..SurfaceSpec::default()
        };
        let surface = MetalSurfaces::new().create(&spec(None)).unwrap();
        let platform = MetalPlatform::process();
        let before = surface.retain_count();
        let lent = platform.surface_device_memory(&surface).unwrap();
        let again = platform.surface_device_memory(&surface).unwrap();
        assert_eq!(lent.identity().key(), again.identity().key());
        assert_eq!(
            lent.length(),
            usize::try_from(surface.alloc_size()).unwrap()
        );
        let io_surface = lent.io_surface().unwrap();
        assert!(io_surface.writable());
        // A Metal surface's ID is its IOSurfaceRef's address (`Handle::id`).
        assert_eq!(io_surface.as_ptr().addr().get() as u64, surface.id().0);
        assert_eq!(surface.retain_count(), before + 2);
        drop((lent, again));
        assert_eq!(surface.retain_count(), before);
        let read_only = MetalSurfaces::new().create(&spec(Some(2))).unwrap();
        assert_eq!(
            platform.surface_device_memory(&read_only).err(),
            Some("a surface the guest made read-only for its clients")
        );
    }
}
