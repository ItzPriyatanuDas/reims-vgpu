//! A surface lent to a device outside Metal, which writes into it itself: the video device's
//! decoders' VideoToolbox, whose pixel transfer writes a decoded frame into a guest's pixel buffer
//! with no copy by the CPU. What is lent is the surface's own IOSurface, the one
//! `MetalSurfaces::create` made over the guest's pages, whose bytes from its base address to its
//! allocation's end are those [`lend_surface_bytes`](super::surface_bytes::lend_surface_bytes)
//! lends the CPU.
//!
//! A lend keeps a reference of the IOSurface for as long as it lives, and its holder keeps it no
//! longer than its own hold on the surface: the video device's `MappedSurface`, a hold the guest
//! sees. The mapper answers the guest's unmap Busy while anything but it retains the IOSurface
//! (core's `iosurface/table.rs`, `unmap`), so the device must release what it made of the surface
//! before the hold goes, as the lend's own reference goes with it.
//!
//! Refused, with why, before any device sees the surface:
//! - **a protected surface**, whose memory the CPU never reaches: protection options set, or no
//!   base address;
//! - **one the guest made read-only for its clients** (`kIOSurfaceClientDirection` 2), over its
//!   pages or the surface's own memory. A device must never write such a surface: this Mac's
//!   scaler wrote into an IOSurface over pages read-only for its clients and the kernel panicked
//!   (`dart-scaler0 ... write protect exception`, the M3 Max, macOS 27.0.1, 26A434, twice on
//!   2026-10-24). Its CPU mapping takes writes where the surface is its own memory
//!   (`super::surface_bytes`), but a device's mapping of it is not measured, so the device is
//!   refused both.
//!
//! A writable surface over the guest's pages is lent ([`CLIENT_MEMORY_LENT`]), unmeasured: no
//! transfer into an IOSurface over client memory has run on this Mac's hardware yet. Setting
//! [`CLIENT_MEMORY_LENT`] false lends such a surface to no device again, and the guest's buffers
//! are then copied by the CPU.
//!
//! [`DeviceLend::writable`] asks the same again, from the surface's record and its own answers,
//! for a device to check before every write.

use std::ffi::c_void;
use std::ptr::NonNull;

use reims_vgpu_core::platform::Handle as _;
use reims_vgpu_core::platform::os::surface::Surface as _;

use super::iosurface::MetalSurface;
use super::surface_bytes::{self, Made, PROTECTED, READ_ONLY};

/// Whether a writable surface over the guest's pages is lent to a device (module docs): yes,
/// though no transfer into one is measured on this Mac.
pub const CLIENT_MEMORY_LENT: bool = true;

/// Why a surface over the guest's pages is not lent while [`CLIENT_MEMORY_LENT`] is false.
pub(super) const CLIENT_MEMORY: &str =
    "a surface over the guest's pages, which no device is lent while CLIENT_MEMORY_LENT is false";

/// A surface's IOSurface lent to a device (module docs).
#[derive(Debug)]
pub struct DeviceLend {
    /// The lend's own reference of the surface's IOSurface.
    surface: MetalSurface,
    /// The bytes from its base address to its allocation's end.
    length: u64,
}

/// Why a device is not lent a surface of these facts, or `None` where it is (module docs).
fn refusal(
    protection_options: u64,
    base_address_is_null: bool,
    made: Made,
) -> Option<&'static str> {
    if protection_options != 0 || base_address_is_null {
        Some(PROTECTED)
    } else if made.read_only_for_clients || made.read_only_direction {
        Some(READ_ONLY)
    } else if made.over_client_memory && !CLIENT_MEMORY_LENT {
        Some(CLIENT_MEMORY)
    } else {
        None
    }
}

/// Why `surface` is not lent to a device now, or `None` where it is.
fn refusal_of(surface: &MetalSurface) -> Option<&'static str> {
    refusal(
        surface.protection_options(),
        surface.base_address().is_null(),
        surface_bytes::made(surface),
    )
}

/// `surface` lent to a device (module docs); `Err` with why where it is not.
pub fn lend_to_devices(surface: &MetalSurface) -> Result<DeviceLend, &'static str> {
    if let Some(why) = refusal_of(surface) {
        return Err(why);
    }
    let offset = surface_bytes::made(surface).offset;
    let length = surface.alloc_size().saturating_sub(offset);
    if length == 0 {
        return Err("a surface with no bytes past its offset");
    }
    Ok(DeviceLend {
        surface: surface.clone_ref(),
        length,
    })
}

impl DeviceLend {
    /// The surface's `IOSurfaceRef`, live while the lend is.
    pub fn io_surface(&self) -> NonNull<c_void> {
        NonNull::from(self.surface.as_iosurface_ref()).cast()
    }

    /// The bytes from the surface's base address to its allocation's end.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Whether a device may write the surface now: asked again, as at the lend, of its record and
    /// its own answers, so that a device checks before every write (module docs).
    pub fn writable(&self) -> bool {
        refusal_of(&self.surface).is_none()
    }
}

#[cfg(test)]
mod tests {
    use reims_vgpu_core::num::ToUsize as _;
    use reims_vgpu_core::platform::os::surface::{IoSurfaces as _, SurfaceSpec};

    use super::*;
    use crate::os::iosurface::MetalSurfaces;

    const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

    /// The `kIOSurfaceClientDirection` core gives a surface read-only for its clients.
    const READ_ONLY_DIRECTION: u32 = 2;

    /// A BGRA surface of its own memory: never one over client memory, which no test of this
    /// module makes (module docs).
    fn own(offset: u64, direction: Option<u32>) -> SurfaceSpec {
        SurfaceSpec {
            pixel_format: BGRA,
            alloc_size: 16384,
            offset,
            width: 64,
            height: (16384 - offset) / 256,
            bytes_per_element: 4,
            bytes_per_row: 256,
            element_width: 1,
            element_height: 1,
            client_direction: direction,
            ..SurfaceSpec::default()
        }
    }

    /// A surface of its own memory is lent whole from its offset, its own IOSurface, and is
    /// writable while it lives; the lend holds one reference of it, and gives it back as it goes
    #[test]
    fn a_surface_is_lent_as_its_own_iosurface() {
        for offset in [0, 0x40] {
            let s = MetalSurfaces::new().create(&own(offset, None)).unwrap();
            let before = s.retain_count();
            let lend = lend_to_devices(&s).unwrap();
            assert_eq!(
                lend.io_surface(),
                NonNull::from(s.as_iosurface_ref()).cast()
            );
            assert_eq!(lend.length(), s.alloc_size() - offset);
            assert!(lend.writable());
            assert_eq!(s.retain_count(), before + 1);
            drop(lend);
            assert_eq!(s.retain_count(), before);
        }
    }

    /// A protected surface, and one the guest made read-only for its clients, over its pages or
    /// its own memory, are lent to no device; the refusal is decided before any device sees them
    #[test]
    fn read_only_and_protected_surfaces_are_not_lent() {
        let made = |read_only_for_clients, read_only_direction| Made {
            offset: 0,
            read_only_for_clients,
            read_only_direction,
            over_client_memory: read_only_for_clients,
        };
        assert_eq!(refusal(0, false, made(false, false)), None);
        assert_eq!(refusal(1, false, made(false, false)), Some(PROTECTED));
        assert_eq!(
            refusal(0x1_0000_0000, false, made(false, false)),
            Some(PROTECTED)
        );
        assert_eq!(refusal(0, true, made(false, false)), Some(PROTECTED));
        assert_eq!(refusal(0, false, made(true, true)), Some(READ_ONLY));
        assert_eq!(refusal(0, false, made(false, true)), Some(READ_ONLY));
        assert_eq!(refusal(1, false, made(true, true)), Some(PROTECTED));
        // Over the guest's pages and writable: lent while CLIENT_MEMORY_LENT is true (module docs).
        let over_pages = Made {
            over_client_memory: true,
            ..made(false, false)
        };
        assert_eq!(
            refusal(0, false, over_pages),
            (!CLIENT_MEMORY_LENT).then_some(CLIENT_MEMORY)
        );
        // A surface of its own memory made read-only for its clients: its record says so, and it
        // is refused, a lookup of it too.
        let s = MetalSurfaces::new()
            .create(&own(0, Some(READ_ONLY_DIRECTION)))
            .unwrap();
        assert!(surface_bytes::made(&s).read_only_direction);
        assert_eq!(lend_to_devices(&s).err(), Some(READ_ONLY));
        let looked_up = s.lookup_by_port().unwrap();
        assert_eq!(lend_to_devices(&looked_up).err(), Some(READ_ONLY));
    }

    /// A lend asks again before each write: a surface whose record comes to say read-only after
    /// the lend is no longer writable
    #[test]
    fn a_lend_asks_again_before_each_write() {
        let s = MetalSurfaces::new().create(&own(0, None)).unwrap();
        let lend = lend_to_devices(&s).unwrap();
        assert!(lend.writable());
        // The record of a read-only surface made again over this one's ID, as a stand-in.
        surface_bytes::record(&s, Some(&own(0, Some(READ_ONLY_DIRECTION))));
        assert!(!lend.writable());
        surface_bytes::record(&s, Some(&own(0, None)));
        assert!(lend.writable());
        assert_eq!(lend.length().to_usize(), s.alloc_size().to_usize());
    }

    /// What the host attaches to a surface is one value for its objects, a lookup's too, and goes
    /// when a surface made here takes its ID; a value of another type is not answered
    #[test]
    fn an_attachment_is_one_value_until_its_id_is_made_again() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let s = MetalSurfaces::new().create(&own(0, None)).unwrap();
        let gone = Arc::new(AtomicBool::new(false));
        let key = |flag: &Flag| Arc::as_ptr(&flag.0).addr();
        let first = surface_bytes::attachment(&s, || Flag(Arc::clone(&gone)), key).unwrap();
        let lookup = s.lookup_by_port().unwrap();
        let again = surface_bytes::attachment(&lookup, || -> Flag { unreachable!() }, key);
        assert_eq!(again, Some(first));
        assert_eq!(surface_bytes::attachment(&s, || 0u8, |_| ()), None);
        assert!(!gone.load(Ordering::SeqCst));
        surface_bytes::record(&s, Some(&own(0, None)));
        assert!(gone.load(Ordering::SeqCst), "gone with its ID made again");
        let made_again =
            surface_bytes::attachment(&s, || Flag(Arc::new(AtomicBool::new(false))), key);
        assert_ne!(made_again, Some(first));
    }
}
