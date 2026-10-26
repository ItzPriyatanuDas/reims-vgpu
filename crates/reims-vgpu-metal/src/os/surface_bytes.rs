//! A surface's bytes and planes as a host outside Metal reaches them: reims-vgpu's video devices,
//! whose decoders write the guest's pixel buffers and whose scaler and encoders read them, in
//! place ([`lend_surface_bytes`], [`lend_surface_bytes_for_reading`], [`plane_layout`]).
//!
//! The CPU reaches a surface's bytes at its base address, between `IOSurfaceLock` and
//! `IOSurfaceUnlock`, as CoreVideo's `CVPixelBufferLockBaseAddress` reaches a pixel buffer's.
//! Two facts a lend needs are not read back from IOSurface, so `MetalSurfaces::create` records
//! them (`record`), by the surface's `IOSurfaceID`, which every object over the surface answers
//! (a port lookup's too):
//! - its bytes' offset into its allocation (`kIOSurfaceOffset`): a lend runs from the base
//!   address to the allocation's end, `IOSurfaceGetAllocSize` less the offset;
//! - whether it was made over the guest's pages read-only for its clients
//!   (`kIOSurfaceClientDirection` 2), which `IOSurfaceCopyValue` answers none for, and whether
//!   it was made with that direction at all, over the guest's pages or its own memory, which a
//!   lend to a device refuses either way (`super::device_lend`).
//!
//! What a host attaches to a surface ([`attachment`]) is kept by the same `IOSurfaceID`: the video
//! device's identity of the surface's mapping, which a device's imports of its memory are kept
//! by. It goes when a surface made here takes the ID, so a surface mapped again never meets the
//! value of the one before, and the entries of surfaces gone go as the table grows, so a value
//! may outlive its surface until then.
//!
//! Measured on the M3 Max, macOS 27.0.1 (26A434), 2026-10-24, by the tests below:
//! - a surface over two ranges of client memory (`kIOSurfaceClientAddressRanges`) of a page
//!   each, apart or following on, has a mapping of its own at its base address, outside the
//!   pages; a write through it under a lock reaches the pages, and a read through it under a
//!   read-only lock sees what the pages hold;
//! - `IOSurfaceCreate` refuses ranges that hold fewer bytes than the allocation (1, 2 and 3 pages
//!   of 4), so a lend never runs past the guest's pages;
//! - one at an offset into its allocation (0x40 and 0x4000) answers the whole allocation's size,
//!   and its base address is that offset into it: the last byte of a lend is the pages' last;
//! - one of its own memory made read-only for its clients takes the CPU's writes through its
//!   base address, as it takes the GPU's (reims-vgpu-vulkan's `os/surface.rs`, `gpu_read_only`);
//! - `IOSurfaceLookup` finds a live surface of this process by its ID, though none is made
//!   global, so the tables keep a live surface's record and attachment as they drop those of
//!   surfaces gone: a surface read-only for its clients stays refused to every writer.
//!
//! One over several pages of client memory read-only for its clients has a read-only mapping of
//! its own at its base address, and a write through it ends the process with a bus error
//! (measured by reims-vgpu-vulkan on the M3 Max, its `os/surface.rs`, `gpu_read_only`), so it is
//! lent for reading alone.

use std::any::Any;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_io_surface::IOSurfaceRef;
use reims_vgpu_core::num::{ToU64, ToUsize};
use reims_vgpu_core::platform::os::surface::{Surface as _, SurfaceSpec};
use reims_vgpu_core::platform::os::vm::KernReturn;
use reims_vgpu_core::sync::Mutex;

use super::iosurface::MetalSurface;

// IOSurface's identity and plane queries (`<IOSurface/IOSurfaceRef.h>`), which core's `Surface`
// does not ask.
#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetID(buffer: &IOSurfaceRef) -> u32;
    /// `IOSurfaceRef IOSurfaceLookup(IOSurfaceID)`: +1 or NULL.
    fn IOSurfaceLookup(csid: u32) -> *mut IOSurfaceRef;
    fn IOSurfaceGetBaseAddressOfPlane(buffer: &IOSurfaceRef, plane_index: usize) -> *mut c_void;
    fn IOSurfaceGetWidthOfPlane(buffer: &IOSurfaceRef, plane_index: usize) -> usize;
    fn IOSurfaceGetHeightOfPlane(buffer: &IOSurfaceRef, plane_index: usize) -> usize;
    fn IOSurfaceGetBytesPerElementOfPlane(buffer: &IOSurfaceRef, plane_index: usize) -> usize;
    fn IOSurfaceGetBytesPerRowOfPlane(buffer: &IOSurfaceRef, plane_index: usize) -> usize;
}

/// `kIOSurfaceLockReadOnly` (`<IOSurface/IOSurfaceTypes.h>`).
const LOCK_READ_ONLY: u32 = 1;

/// The `kIOSurfaceClientDirection` of a surface core makes read-only for its clients (core's
/// `iosurface::descriptor`).
const READ_ONLY_DIRECTION: u32 = 2;

/// Why a lend of a surface whose memory the CPU never reaches is refused.
pub(super) const PROTECTED: &str = "a protected surface, whose memory the CPU never reaches";

/// Why a lend for writing of a surface the guest made read-only for its clients is refused.
pub(super) const READ_ONLY: &str = "a surface the guest made read-only for its clients";

/// One plane of a surface, as a host outside Metal reads it: its bytes' offset from the
/// surface's base address, its size in elements, and its bytes per element and per row, as the
/// surface was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Its bytes' offset from the surface's base address (`IOSurfaceGetBaseAddressOfPlane` less
    /// `IOSurfaceGetBaseAddress`).
    pub offset: u64,
    /// `IOSurfaceGetWidthOfPlane`.
    pub width: u64,
    /// `IOSurfaceGetHeightOfPlane`.
    pub height: u64,
    /// `IOSurfaceGetBytesPerElementOfPlane`.
    pub bytes_per_element: u64,
    /// `IOSurfaceGetBytesPerRowOfPlane`.
    pub bytes_per_row: u64,
}

/// Plane `plane` of `surface`, a surface with planes; `None` for a plane it lacks, and for every
/// plane of a surface without planes, whose layout is the surface's own.
pub fn plane_layout(surface: &MetalSurface, plane: u64) -> Option<PlaneLayout> {
    if plane >= surface.plane_count() {
        return None;
    }
    let index = plane.to_usize();
    let at = surface.as_iosurface_ref();
    // SAFETY: a live IOSurfaceRef, and a plane index under its plane count.
    let (base, width, height, bytes_per_element, bytes_per_row) = unsafe {
        (
            IOSurfaceGetBaseAddressOfPlane(at, index),
            IOSurfaceGetWidthOfPlane(at, index),
            IOSurfaceGetHeightOfPlane(at, index),
            IOSurfaceGetBytesPerElementOfPlane(at, index),
            IOSurfaceGetBytesPerRowOfPlane(at, index),
        )
    };
    let offset = base.addr().checked_sub(surface.base_address().addr())?;
    Some(PlaneLayout {
        offset: offset.to_u64(),
        width: width.to_u64(),
        height: height.to_u64(),
        bytes_per_element: bytes_per_element.to_u64(),
        bytes_per_row: bytes_per_row.to_u64(),
    })
}

/// What a surface was made with that IOSurface does not read back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Made {
    /// Its bytes' offset into its allocation (`kIOSurfaceOffset`).
    pub(super) offset: u64,
    /// Whether it was made over the guest's pages read-only for its clients.
    pub(super) read_only_for_clients: bool,
    /// Whether it was made read-only for its clients (`kIOSurfaceClientDirection` 2), over the
    /// guest's pages or its own memory.
    pub(super) read_only_direction: bool,
    /// Whether it was made over the guest's pages (`kIOSurfaceClientAddressRanges`).
    pub(super) over_client_memory: bool,
}

/// What each surface `MetalSurfaces::create` made at an offset into its allocation, over the
/// guest's pages, or read-only for its clients, was made with, by its `IOSurfaceID`. An ID names one
/// live surface, so a surface made here drops the entry of one gone that had its ID, and entries
/// whose ID no surface holds are dropped as it grows.
struct Records {
    entries: BTreeMap<u32, Made>,
    /// The length at which the next record drops the entries of surfaces gone.
    prune_at: usize,
}

static RECORDS: Mutex<Records> = Mutex::new(Records {
    entries: BTreeMap::new(),
    prune_at: 64,
});

/// `surface`'s `IOSurfaceID`.
fn id(surface: &MetalSurface) -> u32 {
    // SAFETY: a live IOSurfaceRef.
    unsafe { IOSurfaceGetID(surface.as_iosurface_ref()) }
}

/// Whether a surface of `IOSurfaceID` `id` lives.
fn alive(id: u32) -> bool {
    // SAFETY: a lookup by ID, which answers +1 or NULL.
    let found = NonNull::new(unsafe { IOSurfaceLookup(id) });
    // SAFETY: the lookup's +1 reference, handed over and released at once.
    found
        .map(|p| unsafe { MetalSurface::from_retained(p) })
        .is_some()
}

/// Records what `surface`, just made from `spec` (`None`: made otherwise, as a backing surface
/// is), was made with, where a lend needs it; drops the record and the attachment of a surface
/// gone that had its ID either way.
pub(crate) fn record(surface: &MetalSurface, spec: Option<&SurfaceSpec>) {
    let id = id(surface);
    let made = spec.map_or(Made::default(), |spec| Made {
        offset: spec.offset,
        read_only_for_clients: spec.client_address_ranges.is_some()
            && spec.client_direction == Some(READ_ONLY_DIRECTION),
        read_only_direction: spec.client_direction == Some(READ_ONLY_DIRECTION),
        over_client_memory: spec.client_address_ranges.is_some(),
    });
    // Dropped after the lock: what is attached may tell others as it goes.
    let detached = ATTACHED.lock().entries.remove(&id);
    drop(detached);
    let mut records = RECORDS.lock();
    records.entries.remove(&id);
    if made == Made::default() {
        return;
    }
    if records.entries.len() >= records.prune_at {
        records.entries.retain(|&id, _| alive(id));
        records.prune_at = (records.entries.len() * 2).max(64);
    }
    records.entries.insert(id, made);
}

/// What [`record`] holds of `surface`: a surface's default where it holds nothing.
pub(super) fn made(surface: &MetalSurface) -> Made {
    RECORDS
        .lock()
        .entries
        .get(&id(surface))
        .copied()
        .unwrap_or_default()
}

/// What the host attached to each surface, by its `IOSurfaceID` ([`attachment`]; module docs).
struct Attachments {
    entries: BTreeMap<u32, Box<dyn Any + Send + Sync>>,
    /// The length at which the next new entry drops the entries of surfaces gone.
    prune_at: usize,
}

static ATTACHED: Mutex<Attachments> = Mutex::new(Attachments {
    entries: BTreeMap::new(),
    prune_at: 64,
});

/// `read` of the value of type `T` the host attached to `surface`, which `make` makes at the
/// first ask (module docs): one value for every object over the surface, its lookups among them,
/// which goes when a surface made here takes its ID. `None` where a value of another type is
/// attached. `make` and `read` run under the table's lock, so they must call nothing that reaches
/// a surface.
pub fn attachment<T: Any + Send + Sync, R>(
    surface: &MetalSurface,
    make: impl FnOnce() -> T,
    read: impl FnOnce(&T) -> R,
) -> Option<R> {
    let id = id(surface);
    let mut pruned = Vec::new();
    let mut attached = ATTACHED.lock();
    if !attached.entries.contains_key(&id) && attached.entries.len() >= attached.prune_at {
        let gone: Vec<u32> = (attached.entries.keys().copied())
            .filter(|&id| !alive(id))
            .collect();
        pruned.extend(gone.iter().filter_map(|id| attached.entries.remove(id)));
        attached.prune_at = (attached.entries.len() * 2).max(64);
    }
    let value = attached
        .entries
        .entry(id)
        .or_insert_with(|| Box::new(make()));
    let read = value.downcast_ref::<T>().map(read);
    drop(attached);
    // The values of surfaces gone, after the lock: each may tell others as it goes.
    drop(pruned);
    read
}

/// One lend of a surface's bytes at a time in the process, so no two lends' slices alias.
static LENDING: Mutex<()> = Mutex::new(());

/// A surface locked for the CPU, unlocked as it drops, `write` (or a panic in it) done or not.
struct Locked<'a> {
    surface: &'a MetalSurface,
    options: u32,
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        let _ = self.surface.unlock(self.options);
    }
}

/// `surface`, made as `made`, locked with `options`, and its bytes: the first, its base address,
/// and how many run from it to its allocation's end. Refused, with why, for a protected surface
/// and for one IOSurface does not lock.
fn locked(
    surface: &MetalSurface,
    made: Made,
    options: u32,
) -> Result<(Locked<'_>, *mut u8, usize), &'static str> {
    if surface.protection_options() != 0 {
        return Err(PROTECTED);
    }
    if surface.lock(options) != KernReturn::SUCCESS {
        return Err("a surface IOSurfaceLock refuses");
    }
    let held = Locked { surface, options };
    let base = surface.base_address();
    if base.is_null() {
        return Err(PROTECTED);
    }
    let length = surface.alloc_size().saturating_sub(made.offset).to_usize();
    Ok((held, base.as_ptr::<u8>(), length))
}

/// Lends `surface`'s bytes, from its first byte to the end of its allocation, to `write`, under
/// a lock (`IOSurfaceLock` with no option): what `write` leaves in them is in the surface's
/// memory when this returns, in the guest's pages for a surface over them. Refused, with why: a
/// protected surface, whose memory the CPU never reaches, and one the guest made read-only for
/// its clients, whose own mapping is read-only. One lend runs at a time in the process, so
/// `write` must not lend another.
pub fn lend_surface_bytes(
    surface: &MetalSurface,
    write: &mut dyn FnMut(&mut [u8]),
) -> Result<(), &'static str> {
    let made = made(surface);
    if made.read_only_for_clients {
        return Err(READ_ONLY);
    }
    let _one = LENDING.lock();
    let (_held, first, length) = locked(surface, made, 0)?;
    // SAFETY: the locked surface's bytes, `length` from its base address to its allocation's
    // end, readable and writable while the caller holds it (not read-only for its clients);
    // `LENDING` keeps every other lend's slice apart from this one.
    write(unsafe { std::slice::from_raw_parts_mut(first, length) });
    Ok(())
}

/// Lends `surface`'s bytes, laid out as [`lend_surface_bytes`] lends them, to `read`, under a
/// read-only lock (`kIOSurfaceLockReadOnly`), writing nothing back, so a surface the guest made
/// read-only for its clients is lent too. Refused, with why: a protected surface. One lend runs
/// at a time in the process, so `read` must not lend another.
pub fn lend_surface_bytes_for_reading(
    surface: &MetalSurface,
    read: &mut dyn FnMut(&[u8]),
) -> Result<(), &'static str> {
    let made = made(surface);
    let _one = LENDING.lock();
    let (_held, first, length) = locked(surface, made, LOCK_READ_ONLY)?;
    // SAFETY: the locked surface's bytes, `length` from its base address to its allocation's
    // end, readable while the caller holds it; `LENDING` keeps every other lend's slice apart
    // from this one.
    read(unsafe { std::slice::from_raw_parts(first.cast_const(), length) });
    Ok(())
}

#[cfg(test)]
mod tests {
    use reims_vgpu_core::host::{HostPtr, PhysicalMemoryRange};
    use reims_vgpu_core::platform::os::surface::{IoSurfaces, SurfacePlaneSpec};
    use reims_vgpu_core::platform::os::vm::HostVm as _;

    use super::*;
    use crate::os::iosurface::MetalSurfaces;
    use crate::os::vm::MetalVm;

    const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

    /// A BGRA surface of its own memory, 64 texels a row, `alloc_size` bytes.
    fn bgra(alloc_size: u64) -> SurfaceSpec {
        SurfaceSpec {
            pixel_format: BGRA,
            alloc_size,
            width: 64,
            height: alloc_size / 256,
            bytes_per_element: 4,
            bytes_per_row: 256,
            element_width: 1,
            element_height: 1,
            ..SurfaceSpec::default()
        }
    }

    /// Pages of the test's own, as the guest's RAM is the host's: `count` of them.
    struct Pages {
        vm: MetalVm,
        first: HostPtr,
        length: u64,
    }

    impl Pages {
        fn new(count: u64) -> Pages {
            let vm = MetalVm::new();
            let length = vm.page_size() * count;
            let first = vm.allocate_anywhere(length).unwrap();
            Pages { vm, first, length }
        }

        fn page(&self) -> u64 {
            self.vm.page_size()
        }

        /// The ranges of pages `which`, one range a page.
        fn ranges(&self, which: &[u64]) -> Vec<PhysicalMemoryRange> {
            (which.iter())
                .map(|&i| PhysicalMemoryRange {
                    physical_address: self.first.addr().to_u64() + i * self.page(),
                    physical_length: self.page(),
                })
                .collect()
        }

        fn byte(&self, at: u64) -> u8 {
            assert!(at < self.length);
            // SAFETY: the test's own pages, `length` bytes.
            unsafe { self.first.as_ptr::<u8>().add(at.to_usize()).read_volatile() }
        }

        fn set(&self, at: u64, value: u8) {
            assert!(at < self.length);
            // SAFETY: the test's own pages, `length` bytes.
            unsafe {
                self.first
                    .as_ptr::<u8>()
                    .add(at.to_usize())
                    .write_volatile(value)
            }
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            self.vm.deallocate(self.first, self.length);
        }
    }

    /// The length a write lend lends, and what `write` did.
    fn write(surface: &MetalSurface, at: &[(usize, u8)]) -> Result<usize, &'static str> {
        let mut length = 0;
        lend_surface_bytes(surface, &mut |bytes| {
            length = bytes.len();
            for &(i, v) in at {
                bytes[i] = v;
            }
        })?;
        Ok(length)
    }

    /// The bytes at `at` of a read lend, and its length.
    fn read(surface: &MetalSurface, at: &[usize]) -> Result<(usize, Vec<u8>), &'static str> {
        let mut seen = (0, Vec::new());
        lend_surface_bytes_for_reading(surface, &mut |bytes| {
            seen = (bytes.len(), at.iter().map(|&i| bytes[i]).collect());
        })?;
        Ok(seen)
    }

    /// A surface of its own memory is lent from its base address to its allocation's end, and
    /// what a write lend leaves a read lend sees
    #[test]
    fn a_surface_of_its_own_memory_is_lent_whole() {
        let s = MetalSurfaces::new().create(&bgra(16384)).unwrap();
        let length = s.alloc_size().to_usize();
        assert_eq!(write(&s, &[(0, 0x11), (length - 1, 0x22)]), Ok(length));
        assert_eq!(read(&s, &[0, length - 1]), Ok((length, vec![0x11, 0x22])));
    }

    /// A surface over pages of client memory, apart or following on, has a mapping of its own at
    /// its base address; what a write lend writes reaches the pages, and a read lend sees them
    #[test]
    fn a_surface_over_the_guests_pages_is_lent_through_its_mapping_of_them() {
        for which in [[0, 2], [0, 1]] {
            let pages = Pages::new(3);
            let page = pages.page();
            let spec = SurfaceSpec {
                client_address_ranges: Some(pages.ranges(&which)),
                ..bgra(2 * page)
            };
            let s = MetalSurfaces::new().create(&spec).unwrap();
            let base = s.base_address().addr().to_u64();
            let first = pages.first.addr().to_u64();
            assert!(!(first..first + pages.length).contains(&base), "{which:?}");
            let (second, length) = ((page + 7).to_usize(), (2 * page).to_usize());
            assert_eq!(write(&s, &[(5, 0xab), (second, 0xcd)]), Ok(length));
            assert_eq!(pages.byte(5), 0xab);
            assert_eq!(pages.byte(which[1] * page + 7), 0xcd);
            pages.set(9, 0x5a);
            pages.set(which[1] * page + 3, 0xa5);
            let seen = read(&s, &[9, (page + 3).to_usize()]);
            assert_eq!(seen, Ok((length, vec![0x5a, 0xa5])), "{which:?}");
        }
    }

    /// A surface over fewer pages than its allocation is never made, so no lend runs past them
    #[test]
    fn a_surface_over_fewer_pages_than_its_allocation_is_refused() {
        let pages = Pages::new(4);
        for count in 1..4 {
            let which: Vec<u64> = (0..count).collect();
            let spec = SurfaceSpec {
                client_address_ranges: Some(pages.ranges(&which)),
                ..bgra(pages.length)
            };
            assert!(MetalSurfaces::new().create(&spec).is_none(), "{count}");
        }
        let spec = SurfaceSpec {
            client_address_ranges: Some(pages.ranges(&[0, 1, 2, 3])),
            ..bgra(pages.length)
        };
        assert!(MetalSurfaces::new().create(&spec).is_some());
    }

    /// A surface at an offset into its allocation is lent from its base address, that offset
    /// into the pages, to their end
    #[test]
    fn an_offset_surface_is_lent_from_its_offset_to_its_allocations_end() {
        for offset in [0x40, 0x4000] {
            let pages = Pages::new(2);
            let spec = SurfaceSpec {
                offset,
                height: (pages.length - offset) / 256,
                client_address_ranges: Some(pages.ranges(&[0, 1])),
                ..bgra(pages.length)
            };
            let s = MetalSurfaces::new().create(&spec).unwrap();
            assert_eq!(s.alloc_size(), pages.length);
            let length = (pages.length - offset).to_usize();
            assert_eq!(write(&s, &[(0, 0x31), (length - 1, 0x32)]), Ok(length));
            assert_eq!(pages.byte(offset), 0x31, "{offset:#x}");
            assert_eq!(pages.byte(pages.length - 1), 0x32, "{offset:#x}");
            assert_eq!(read(&s, &[0]), Ok((length, vec![0x31])));
        }
    }

    /// A surface the guest made read-only for its clients is lent for reading alone, through a
    /// lookup of it too; one of its own memory so made takes the CPU's writes
    #[test]
    fn a_surface_read_only_for_its_clients_is_lent_for_reading_alone() {
        let pages = Pages::new(3);
        let page = pages.page();
        let spec = SurfaceSpec {
            client_direction: Some(READ_ONLY_DIRECTION),
            client_address_ranges: Some(pages.ranges(&[0, 2])),
            ..bgra(2 * page)
        };
        let s = MetalSurfaces::new().create(&spec).unwrap();
        let looked_up = s.lookup_by_port().unwrap();
        pages.set(2 * page + 1, 0x77);
        for s in [&s, &looked_up] {
            assert!(write(s, &[]).is_err());
            assert_eq!(
                read(s, &[(page + 1).to_usize()]),
                Ok(((2 * page).to_usize(), vec![0x77]))
            );
        }
        let own = SurfaceSpec {
            client_direction: Some(READ_ONLY_DIRECTION),
            ..bgra(16384)
        };
        let s = MetalSurfaces::new().create(&own).unwrap();
        assert_eq!(write(&s, &[(3, 0x44)]), Ok(s.alloc_size().to_usize()));
        assert_eq!(read(&s, &[3]).map(|(_, b)| b), Ok(vec![0x44]));
    }

    /// A record goes with the surface it was made for: a surface made otherwise over its ID has
    /// none
    #[test]
    fn a_surface_made_again_drops_the_record_of_its_id() {
        let pages = Pages::new(1);
        let spec = SurfaceSpec {
            client_direction: Some(READ_ONLY_DIRECTION),
            client_address_ranges: Some(pages.ranges(&[0])),
            ..bgra(pages.length)
        };
        let s = MetalSurfaces::new().create(&spec).unwrap();
        assert!(made(&s).read_only_for_clients);
        record(&s, None);
        assert_eq!(made(&s), Made::default());
    }

    /// The records and attachments of surfaces gone are dropped as the tables grow, and a live
    /// surface's are kept through it: a surface read-only for its clients stays refused to every
    /// writer, and keeps the value attached to it
    #[test]
    fn a_live_surfaces_record_outlives_the_records_of_surfaces_gone() {
        let pages = Pages::new(1);
        let spec = SurfaceSpec {
            client_direction: Some(READ_ONLY_DIRECTION),
            client_address_ranges: Some(pages.ranges(&[0])),
            ..bgra(pages.length)
        };
        let kept = MetalSurfaces::new().create(&spec).unwrap();
        assert_eq!(attachment(&kept, || 7u32, |v| *v), Some(7));
        for _ in 0..200 {
            let gone = MetalSurfaces::new()
                .create(&SurfaceSpec {
                    offset: 0x40,
                    height: (16384 - 0x40) / 256,
                    ..bgra(16384)
                })
                .unwrap();
            assert_eq!(made(&gone).offset, 0x40);
            assert_eq!(attachment(&gone, || 0u32, |v| *v), Some(0));
        }
        assert!(
            RECORDS.lock().entries.len() < 200,
            "the records of surfaces gone are dropped"
        );
        assert!(ATTACHED.lock().entries.len() < 200, "and their attachments");
        assert!(made(&kept).read_only_for_clients);
        assert_eq!(write(&kept, &[]), Err(READ_ONLY));
        assert_eq!(attachment(&kept, || 8u32, |v| *v), Some(7));
    }

    /// Each plane of a two-plane surface is where it was made, and a surface without planes has
    /// no plane layout
    #[test]
    fn planes_are_as_the_surface_was_made() {
        let plane = |offset, width, height, bytes_per_element, bytes_per_row| SurfacePlaneSpec {
            base: offset,
            offset,
            width,
            height,
            size: bytes_per_row * height,
            bytes_per_row,
            bytes_per_element,
            element_width: 1,
            element_height: 1,
            ..SurfacePlaneSpec::default()
        };
        let spec = SurfaceSpec {
            planes: Some(vec![plane(0, 64, 32, 1, 64), plane(4096, 32, 16, 2, 64)]),
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
        let s = MetalSurfaces::new().create(&spec).unwrap();
        let layout = |offset, width, height, bytes_per_element, bytes_per_row| PlaneLayout {
            offset,
            width,
            height,
            bytes_per_element,
            bytes_per_row,
        };
        assert_eq!(plane_layout(&s, 0), Some(layout(0, 64, 32, 1, 64)));
        assert_eq!(plane_layout(&s, 1), Some(layout(4096, 32, 16, 2, 64)));
        assert_eq!(plane_layout(&s, 2), None);
        let s = MetalSurfaces::new().create(&bgra(16384)).unwrap();
        assert_eq!(plane_layout(&s, 0), None);
    }
}
