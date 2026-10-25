//! The writer's checks, each shown to keep a buffer from CoreVideo and
//! VideoToolbox with stand-ins that fail the test if it reaches them, and
//! its way through to the transfer for an ordinary IOSurface of the
//! guest's layout.
//!
//! Every surface here is of its own memory, made by IOSurface or CoreVideo:
//! none is over pages of client memory (`kIOSurfaceClientAddressRanges`).
//! Only `a_transfer_writes_what_the_cpu_copies` and `how_long_a_write_takes`
//! transfer on the host's hardware, into ordinary surfaces alone. The
//! read-only mappings are the test's own pages, which only `mach_vm_region`
//! reads.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use reims_vgpu_video_core::device_memory::{DeviceMemory, MemoryIdentity, MemoryLife};
use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::surfaces::{PlaneLayout, SurfaceError, SurfaceLayout};

use super::*;
use crate::macos::cf::{self, Cf, Owned, constant};
use crate::macos::ffi::core_foundation::CFGetRetainCount;
use crate::macos::ffi::io_surface::{IOSurfaceCreate, IOSurfaceGetUseCount};
use crate::macos::ffi::mach::{
    VM_FLAGS_ANYWHERE, VM_PROT_READ, mach_vm_address_t, mach_vm_allocate, mach_vm_deallocate,
    mach_vm_protect,
};

/// A guest buffer's layout as `Dpba` asks, as the conformance suite's
/// stand-in guest lays it out: rows 256 bytes past a 64-byte-aligned row,
/// the chroma plane at a 256-byte boundary.
fn guest_layout(format: PixelFormat, width: u32, height: u32) -> (SurfaceLayout, usize) {
    let sample = format.bytes_per_sample();
    let bytes_per_row = (width as usize * sample).next_multiple_of(64) + 256;
    let chroma = (bytes_per_row * height as usize).next_multiple_of(256);
    let length = chroma + bytes_per_row * height.div_ceil(2) as usize;
    let layout = SurfaceLayout {
        pixel_format: format.fourcc(),
        width,
        height,
        planes: vec![
            PlaneLayout {
                offset: 0,
                bytes_per_row,
                width,
                height,
            },
            PlaneLayout {
                offset: chroma,
                bytes_per_row,
                width: width.div_ceil(2),
                height: height.div_ceil(2),
            },
        ],
        bytes_per_row: 0,
    };
    (layout, length)
}

/// An IOSurface of its own memory laid out as `layout`, `length` bytes, as
/// IOSurface makes one from a plane list: never over client memory.
pub(crate) fn ordinary_surface(layout: &SurfaceLayout, length: usize) -> Result<Owned, String> {
    let number = |value: usize| cf::number_i64(i64::try_from(value).unwrap_or(i64::MAX));
    let sample =
        PixelFormat::from_fourcc(layout.pixel_format).map_or(1, PixelFormat::bytes_per_sample);
    let planes: Vec<Owned> = (layout.planes.iter().enumerate())
        .map(|(index, plane)| {
            let values = [
                number(plane.width as usize),
                number(plane.height as usize),
                number(plane.bytes_per_row),
                number(sample * if index == 0 { 1 } else { 2 }),
                number(1),
                number(1),
                number(plane.offset),
                number(plane.bytes_per_row * plane.height as usize),
            ];
            let keys = [
                constant!(io_surface::kIOSurfacePlaneWidth),
                constant!(io_surface::kIOSurfacePlaneHeight),
                constant!(io_surface::kIOSurfacePlaneBytesPerRow),
                constant!(io_surface::kIOSurfacePlaneBytesPerElement),
                constant!(io_surface::kIOSurfacePlaneElementWidth),
                constant!(io_surface::kIOSurfacePlaneElementHeight),
                constant!(io_surface::kIOSurfacePlaneOffset),
                constant!(io_surface::kIOSurfacePlaneSize),
            ];
            let entries: Vec<(Cf<'_>, Cf<'_>)> = keys
                .into_iter()
                .zip(&values)
                .map(|(k, v)| (k, v.cf()))
                .collect();
            cf::dictionary(&entries)
        })
        .collect();
    let plane_list: Vec<_> = planes.iter().map(Owned::cf).collect();
    let plane_list = cf::array(&plane_list);
    let values = [
        number(layout.width as usize),
        number(layout.height as usize),
        cf::number_i64(i64::from(layout.pixel_format.0)),
        number(length),
    ];
    let keys = [
        constant!(io_surface::kIOSurfaceWidth),
        constant!(io_surface::kIOSurfaceHeight),
        constant!(io_surface::kIOSurfacePixelFormat),
        constant!(io_surface::kIOSurfaceAllocSize),
    ];
    let mut entries: Vec<(Cf<'_>, Cf<'_>)> = keys
        .into_iter()
        .zip(&values)
        .map(|(k, v)| (k, v.cf()))
        .collect();
    entries.push((constant!(io_surface::kIOSurfacePlaneInfo), plane_list.cf()));
    let properties = cf::dictionary(&entries);
    // SAFETY: a live dictionary of IOSurface's keys, none of client memory.
    let made = unsafe { IOSurfaceCreate(properties.as_ptr()) };
    // SAFETY: a Create call's result.
    unsafe { Owned::take(made.cast_const()) }
        .ok_or_else(|| "IOSurfaceCreate made no surface".into())
}

/// A held buffer's memory as a stand-in host lends it: `surface` as its
/// IOSurface, with `writable` as the host's word.
struct Lent {
    life: MemoryLife,
    length: usize,
    surface: NonNull<c_void>,
    writable: bool,
}

// SAFETY: the surface is a CoreFoundation object any thread may use.
unsafe impl Send for Lent {}
// SAFETY: as above.
unsafe impl Sync for Lent {}

impl Lent {
    fn new(surface: NonNull<c_void>, length: usize, writable: bool) -> Self {
        Self {
            life: MemoryLife::new(),
            length,
            surface,
            writable,
        }
    }
}

impl DeviceMemory for Lent {
    fn identity(&self) -> MemoryIdentity {
        self.life.identity()
    }

    fn length(&self) -> usize {
        self.length
    }

    fn io_surface(&self) -> Result<LentIoSurface<'_>, SurfaceError> {
        // SAFETY: the test keeps the surface live past this memory.
        Ok(unsafe { LentIoSurface::new(self.surface, self.writable) })
    }
}

/// A transfer that counts what reaches it, each a failure of the test, and
/// refuses it.
#[derive(Default)]
struct Never(AtomicUsize);

impl Transfer for Never {
    fn transfer(&self, _: &PixelBuffer, _: &mut PixelBuffer) -> Result<(), OSStatus> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(-1)
    }
}

/// A transfer that counts what reaches it and writes nothing.
#[derive(Default)]
struct Counting(AtomicUsize);

impl Transfer for Counting {
    fn transfer(&self, _: &PixelBuffer, _: &mut PixelBuffer) -> Result<(), OSStatus> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A destination of the given answers that counts each pixel buffer
/// asked of it, each a failure of the test, and makes none, so that what
/// the writer declines is shown never to reach CoreVideo either.
struct StandIn {
    base: *mut c_void,
    alloc: usize,
    format: OSType,
    size: (usize, usize),
    planes: Vec<SurfacePlane>,
    asked: AtomicUsize,
}

impl StandIn {
    /// Answers as `layout` over `length` bytes at `base`.
    fn of(layout: &SurfaceLayout, length: usize, base: *mut c_void) -> Self {
        let planes = (layout.planes.iter())
            .map(|plane| SurfacePlane {
                offset: plane.offset,
                bytes_per_row: plane.bytes_per_row,
                width: plane.width as usize,
                height: plane.height as usize,
            })
            .collect();
        Self {
            base,
            alloc: length,
            format: layout.pixel_format.0,
            size: (layout.width as usize, layout.height as usize),
            planes,
            asked: AtomicUsize::new(0),
        }
    }
}

impl Destination for StandIn {
    fn base_address(&self) -> *mut c_void {
        self.base
    }
    fn alloc_size(&self) -> usize {
        self.alloc
    }
    fn pixel_format(&self) -> OSType {
        self.format
    }
    fn size(&self) -> (usize, usize) {
        self.size
    }
    fn planes(&self) -> Vec<SurfacePlane> {
        self.planes.clone()
    }
    fn pixel_buffer(&self) -> Result<PixelBuffer, String> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Err("the stand-in makes no pixel buffer".into())
    }
}

/// Pages of the test's own, `length` bytes rounded up to pages, the
/// protection of each settable; deallocated on drop.
struct Pages {
    at: mach_vm_address_t,
    length: u64,
}

impl Pages {
    fn new(length: usize) -> Result<Self, String> {
        let mut at: mach_vm_address_t = 0;
        // SAFETY: the process's own task, a live local for the address.
        let status = unsafe {
            mach_vm_allocate(
                mach_task_self_,
                &raw mut at,
                length as u64,
                VM_FLAGS_ANYWHERE,
            )
        };
        if status != KERN_SUCCESS {
            return Err(format!("mach_vm_allocate answered {status}"));
        }
        Ok(Self {
            at,
            length: length as u64,
        })
    }

    fn base(&self) -> *mut c_void {
        std::ptr::with_exposed_provenance_mut(self.at as usize)
    }

    /// The pages as a stand-in's IOSurface, which nothing dereferences.
    fn stand_in(&self) -> Result<NonNull<c_void>, String> {
        NonNull::new(self.base()).ok_or_else(|| "no pages".into())
    }

    /// Makes `length` bytes from `offset` read-only, whole pages.
    fn read_only(&self, offset: u64, length: u64) -> Result<(), String> {
        // SAFETY: inside the test's own allocation.
        let status =
            unsafe { mach_vm_protect(mach_task_self_, self.at + offset, length, 0, VM_PROT_READ) };
        if status == KERN_SUCCESS {
            Ok(())
        } else {
            Err(format!("mach_vm_protect answered {status}"))
        }
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        // SAFETY: the allocation `new` made, freed once.
        unsafe { mach_vm_deallocate(mach_task_self_, self.at, self.length) };
    }
}

/// An ordinary 420v frame of `width` by `height`, as a decoder's output.
fn frame(format: PixelFormat, width: u32, height: u32) -> Result<PixelBuffer, String> {
    PixelBuffer::new(format.fourcc().0, width as usize, height as usize)
        .map_err(|error| error.to_string())
}

fn source(image: &PixelBuffer, format: PixelFormat) -> Source<'_> {
    let (width, height) = image.size();
    Source {
        image,
        format,
        width: width as u32,
        height: height as u32,
    }
}

fn reason(written: Result<DeviceWrite, BackendError>) -> Result<String, String> {
    match written {
        Ok(DeviceWrite::Declined(why)) => Ok(why),
        other => Err(format!("not declined: {other:?}")),
    }
}

const NV12: PixelFormat = PixelFormat::Nv12VideoRange;

/// A buffer the host says it would not write, read-only for its clients or
/// protected, is declined before its surface is read at all: neither
/// CoreVideo nor VideoToolbox sees it
#[test]
fn a_buffer_the_host_would_not_write_never_reaches_videotoolbox() -> Result<(), String> {
    let (layout, length) = guest_layout(NV12, 64, 48);
    let pages = Pages::new(length)?;
    let lent = Lent::new(pages.stand_in()?, length, false);
    let target = GuestTarget::new(&layout, &lent)?;
    let image = frame(NV12, 64, 48)?;
    let destination = StandIn::of(&layout, length, pages.base());
    let never = Never::default();
    let lent_surface = lent.io_surface().map_err(|e| e.to_string())?;
    let why = reason(write_into(
        &source(&image, NV12),
        &target,
        lent_surface,
        &destination,
        &never,
    ))?;
    assert!(why.contains("would not write"), "{why}");
    // Through the writer's own entry too, which would read the surface the
    // host lent: it reads nothing of it.
    let why = reason(write(&source(&image, NV12), &target, &never))?;
    assert!(why.contains("would not write"), "{why}");
    assert_eq!(
        destination.asked.load(Ordering::SeqCst),
        0,
        "reached CoreVideo"
    );
    assert_eq!(never.0.load(Ordering::SeqCst), 0, "reached VideoToolbox");
    Ok(())
}

/// A protected buffer, whose IOSurface has no base address, is declined
/// before CoreVideo or VideoToolbox sees it
#[test]
fn a_protected_buffer_never_reaches_videotoolbox() -> Result<(), String> {
    let (layout, length) = guest_layout(NV12, 64, 48);
    let pages = Pages::new(length)?;
    let lent = Lent::new(pages.stand_in()?, length, true);
    let target = GuestTarget::new(&layout, &lent)?;
    let image = frame(NV12, 64, 48)?;
    let destination = StandIn::of(&layout, length, std::ptr::null_mut());
    let never = Never::default();
    let lent_surface = lent.io_surface().map_err(|e| e.to_string())?;
    let why = reason(write_into(
        &source(&image, NV12),
        &target,
        lent_surface,
        &destination,
        &never,
    ))?;
    assert!(why.contains("protected"), "{why}");
    assert_eq!(
        destination.asked.load(Ordering::SeqCst),
        0,
        "reached CoreVideo"
    );
    assert_eq!(never.0.load(Ordering::SeqCst), 0, "reached VideoToolbox");
    Ok(())
}

/// A buffer whose mapping is read-only, wholly or in its last page alone,
/// is declined before CoreVideo or VideoToolbox sees it, though the host
/// called it writable: the third check is the process's own mapping, not
/// the host's word
#[test]
fn a_read_only_mapping_never_reaches_videotoolbox() -> Result<(), String> {
    let (layout, length) = guest_layout(NV12, 640, 360);
    let image = frame(NV12, 640, 360)?;
    let page = 16384;
    let last = (length as u64 - 1) / page * page;
    for (offset, protected) in [(0, length as u64), (last, 1)] {
        let pages = Pages::new(length)?;
        pages.read_only(offset, protected)?;
        let lent = Lent::new(pages.stand_in()?, length, true);
        let target = GuestTarget::new(&layout, &lent)?;
        let destination = StandIn::of(&layout, length, pages.base());
        let never = Never::default();
        let lent_surface = lent.io_surface().map_err(|e| e.to_string())?;
        let why = reason(write_into(
            &source(&image, NV12),
            &target,
            lent_surface,
            &destination,
            &never,
        ))?;
        assert!(why.contains("not writable"), "{offset:#x}: {why}");
        assert_eq!(
            destination.asked.load(Ordering::SeqCst),
            0,
            "reached CoreVideo"
        );
        assert_eq!(never.0.load(Ordering::SeqCst), 0, "reached VideoToolbox");
    }
    // The same pages left writable pass the check.
    let pages = Pages::new(length)?;
    assert_eq!(writable_mapping(pages.base().addr(), length), Ok(()));
    // Bytes not mapped at all fail it.
    let at = pages.base().addr();
    drop(pages);
    assert!(writable_mapping(at, length).is_err());
    Ok(())
}

/// A buffer whose surface is not its layout, or of another format or size
/// than the frame, is declined before VideoToolbox sees it
#[test]
fn a_buffer_not_of_the_frame_or_its_layout_never_reaches_videotoolbox() -> Result<(), String> {
    let (layout, length) = guest_layout(NV12, 64, 48);
    let pages = Pages::new(length)?;
    let lent = Lent::new(pages.stand_in()?, length, true);
    let target = GuestTarget::new(&layout, &lent)?;
    let lent_surface = lent.io_surface().map_err(|e| e.to_string())?;
    let image = frame(NV12, 64, 48)?;
    let never = Never::default();
    let declined = |source: &Source<'_>, destination: &StandIn| -> Result<String, String> {
        let why = reason(write_into(
            source,
            &target,
            lent_surface,
            destination,
            &never,
        ))?;
        assert_eq!(
            destination.asked.load(Ordering::SeqCst),
            0,
            "reached CoreVideo: {why}"
        );
        Ok(why)
    };
    let destination = StandIn::of(&layout, length, pages.base());
    // A full-range frame into a video-range buffer: a transfer converts.
    let full = frame(PixelFormat::Nv12FullRange, 64, 48)?;
    let why = declined(&source(&full, PixelFormat::Nv12FullRange), &destination)?;
    assert!(why.contains("convert"), "{why}");
    // A frame smaller or larger than the buffer: the CPU copies what fits.
    for (width, height) in [(32, 48), (64, 64)] {
        let other = frame(NV12, width, height)?;
        let why = declined(&source(&other, NV12), &destination)?;
        assert!(why.contains("whole buffers"), "{why}");
    }
    // A picture smaller than its frame's buffer.
    let cut = Source {
        width: 60,
        ..source(&image, NV12)
    };
    let why = declined(&cut, &destination)?;
    assert!(why.contains("whole buffers"), "{why}");
    // A surface whose planes are not the layout's, or of another format, or
    // smaller than the memory lent.
    let mut moved = StandIn::of(&layout, length, pages.base());
    moved.planes[1].offset += 256;
    let why = declined(&source(&image, NV12), &moved)?;
    assert!(why.contains("planes"), "{why}");
    let mut short = StandIn::of(&layout, length, pages.base());
    short.planes[1].height -= 1;
    let why = declined(&source(&image, NV12), &short)?;
    assert!(why.contains("planes"), "{why}");
    let mut other = StandIn::of(&layout, length, pages.base());
    other.format = PixelFormat::Nv12FullRange.fourcc().0;
    let why = declined(&source(&image, NV12), &other)?;
    assert!(why.contains("IOSurface is 420f"), "{why}");
    let mut small = StandIn::of(&layout, length, pages.base());
    small.alloc -= 1;
    let why = declined(&source(&image, NV12), &small)?;
    assert!(why.contains("fewer"), "{why}");
    assert_eq!(never.0.load(Ordering::SeqCst), 0, "reached VideoToolbox");
    Ok(())
}

/// An ordinary IOSurface of the guest's layout, its own memory, passes
/// every check and reaches the transfer once, through a pixel buffer over
/// it at its planes; the pixel buffer's reference and use of it are gone
/// when the write returns. 10-bit too; an odd size is declined before the
/// transfer.
#[test]
fn an_ordinary_writable_surface_reaches_the_transfer_once() -> Result<(), String> {
    for (format, width, height, reaches) in [
        (NV12, 64, 48, true),
        (PixelFormat::P010VideoRange, 64, 48, true),
        (NV12, 1366, 766, true),
        (NV12, 641, 361, false),
        (NV12, 640, 361, false),
        (PixelFormat::P010VideoRange, 643, 364, false),
    ] {
        let (layout, length) = guest_layout(format, width, height);
        let surface = ordinary_surface(&layout, length)?;
        // SAFETY: a live IOSurface.
        let retained = || unsafe { CFGetRetainCount(surface.as_ptr()) };
        // SAFETY: a live IOSurface.
        let used = || unsafe { IOSurfaceGetUseCount(surface.as_mut_ptr()) };
        let before = (retained(), used());
        let pointer = NonNull::new(surface.as_mut_ptr()).ok_or("no surface")?;
        let lent = Lent::new(pointer, length, true);
        let target = GuestTarget::new(&layout, &lent)?;
        let image = frame(format, width, height)?;
        let counting = Counting::default();
        let written = write(&source(&image, format), &target, &counting);
        if reaches {
            assert_eq!(
                written.ok(),
                Some(DeviceWrite::Written),
                "{format:?} {width}x{height}"
            );
        } else {
            let why = reason(written)?;
            assert!(why.contains("odd size"), "{why}");
        }
        assert_eq!(counting.0.load(Ordering::SeqCst), usize::from(reaches));
        assert_eq!(
            (retained(), used()),
            before,
            "the pixel buffer's reference or use outlived the write"
        );
    }
    Ok(())
}

/// The layouts these tests give IOSurface are the guest's: its planes as
/// laid out, its format, its size
#[test]
fn an_ordinary_surface_is_laid_out_as_asked() -> Result<(), String> {
    let (layout, length) = guest_layout(NV12, 641, 361);
    let surface = ordinary_surface(&layout, length)?;
    let pointer = NonNull::new(surface.as_mut_ptr()).ok_or("no surface")?;
    let lent = Lent::new(pointer, length, true);
    let io_surface = IoSurface::of(lent.io_surface().map_err(|e| e.to_string())?);
    assert_eq!(io_surface.pixel_format(), FourCc::new(*b"420v").0);
    assert_eq!(io_surface.size(), (641, 361));
    assert!(io_surface.alloc_size() >= length);
    let planes: Vec<_> = (io_surface.planes().iter())
        .map(|p| (p.offset, p.bytes_per_row, p.width, p.height))
        .collect();
    let asked: Vec<_> = (layout.planes.iter())
        .map(|p| {
            (
                p.offset,
                p.bytes_per_row,
                p.width as usize,
                p.height as usize,
            )
        })
        .collect();
    assert_eq!(planes, asked);
    Ok(())
}

/// `f` given `surface`'s bytes, `length` from its base address, under a
/// lock: read-only where `read_only`.
fn with_surface_bytes<T>(
    surface: &Owned,
    length: usize,
    read_only: bool,
    f: impl FnOnce(&mut [u8]) -> T,
) -> Result<T, String> {
    use crate::macos::ffi::io_surface::{IOSurfaceLock, IOSurfaceUnlock, kIOSurfaceLockReadOnly};
    let options = if read_only { kIOSurfaceLockReadOnly } else { 0 };
    // SAFETY: a live IOSurface; a NULL seed is allowed.
    let status = unsafe { IOSurfaceLock(surface.as_mut_ptr(), options, std::ptr::null_mut()) };
    if status != KERN_SUCCESS {
        return Err(format!("IOSurfaceLock answered {status}"));
    }
    // SAFETY: a live IOSurface.
    let base = unsafe { IOSurfaceGetBaseAddress(surface.as_mut_ptr()) };
    // SAFETY: the locked surface's own memory, `length` bytes of its
    // allocation from its base address, written only where not read-only.
    let result = (!base.is_null())
        .then(|| f(unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), length) }));
    // SAFETY: locked above with these options.
    unsafe { IOSurfaceUnlock(surface.as_mut_ptr(), options, std::ptr::null_mut()) };
    result.ok_or_else(|| "the surface has no base address".into())
}

/// A frame of `format` and size whose samples run through every value a
/// sample may hold: 0 to 255, or every 10-bit value in a 16-bit sample's
/// upper bits, as CoreVideo's `x420` and `xf20` hold them.
fn pattern(format: PixelFormat, width: u32, height: u32) -> Result<PixelBuffer, String> {
    let mut image = frame(format, width, height)?;
    let sample = format.bytes_per_sample();
    image
        .write(|planes| {
            for (index, (bytes, geometry)) in planes.iter_mut().enumerate() {
                for row in 0..geometry.height {
                    let line = &mut bytes[row * geometry.bytes_per_row..][..geometry.bytes_per_row];
                    for (at, value) in line.chunks_mut(sample).enumerate() {
                        let v = (at * 7 + row * 13 + index * 101) as u32;
                        if sample == 1 {
                            value[0] = v as u8;
                        } else {
                            value.copy_from_slice(&(((v % 1024) << 6) as u16).to_le_bytes());
                        }
                    }
                }
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(image)
}

/// The bytes `layout`'s planes hold of a frame of its size: each plane's
/// rows of samples, and the bytes outside them, its rows' padding and the
/// gap between the planes, apart.
fn split_samples(layout: &SurfaceLayout, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let sample =
        PixelFormat::from_fourcc(layout.pixel_format).map_or(1, PixelFormat::bytes_per_sample);
    let width = layout.width as usize;
    let rows = [layout.height as usize, layout.height.div_ceil(2) as usize];
    let row_bytes = [width * sample, width.div_ceil(2) * 2 * sample];
    let mut inside = vec![false; bytes.len()];
    for (index, plane) in layout.planes.iter().enumerate() {
        for row in 0..rows[index] {
            let at = plane.offset + row * plane.bytes_per_row;
            inside[at..at + row_bytes[index]].fill(true);
        }
    }
    let (mut samples, mut rest) = (Vec::new(), Vec::new());
    for (byte, inside) in bytes.iter().zip(inside) {
        if inside {
            samples.push(*byte)
        } else {
            rest.push(*byte)
        }
    }
    (samples, rest)
}

/// What a buffer holds before a frame is written into it.
const POISON: u8 = 0xa5;

/// A frame written by VideoToolbox's pixel transfer into an ordinary
/// IOSurface of the guest's layout, its own memory, is the CPU's copy of
/// it, sample for sample, at 8 and 10 bits, video and full range, from
/// 64x48 to 3840x2160; the transfer leaves every byte outside the samples
/// as it was, as the CPU's copy does; and the surface's references and
/// use count are what they were once the write returns, so no hold
/// outlives it. An odd
/// size is declined, which the transfer writes otherwise (`write_into`).
/// This transfers on the host's hardware into ordinary surfaces alone: run
/// it under the Mac's GPU lock.
#[test]
fn a_transfer_writes_what_the_cpu_copies() -> Result<(), String> {
    use reims_vgpu_video_core::backend::DecodedFrame as _;

    use crate::macos::decoder::Frame;

    let transfer = std::sync::Arc::new(PixelTransfer::default());
    for (format, width, height) in [
        (NV12, 64, 48),
        (PixelFormat::Nv12FullRange, 1920, 1080),
        (NV12, 1366, 766),
        (NV12, 3840, 2160),
        (PixelFormat::P010VideoRange, 1280, 720),
        (PixelFormat::P010FullRange, 642, 362),
        (PixelFormat::P010VideoRange, 3840, 2160),
        (NV12, 641, 361),
        (PixelFormat::P010VideoRange, 643, 363),
    ] {
        let name = format!("{} {width}x{height}", format.fourcc());
        let (layout, length) = guest_layout(format, width, height);
        let image = pattern(format, width, height)?;
        let frame =
            Frame::new(7, image, format, None, &transfer).map_err(|error| error.to_string())?;
        // The CPU's copy, into memory of the guest's layout.
        let mut copied = vec![POISON; length];
        {
            let mut planes = layout.split(&mut copied).map_err(|e| e.to_string())?;
            (frame.read_back().map_err(|e| e.to_string())?)
                .copy_to(&mut planes)
                .map_err(|e| e.to_string())?;
        }
        // The transfer's, into an ordinary IOSurface of the same layout.
        let surface = ordinary_surface(&layout, length)?;
        with_surface_bytes(&surface, length, false, |bytes| bytes.fill(POISON))?;
        // SAFETY: a live IOSurface.
        let retained = || unsafe { CFGetRetainCount(surface.as_ptr()) };
        // SAFETY: a live IOSurface.
        let used = || unsafe { IOSurfaceGetUseCount(surface.as_mut_ptr()) };
        let before = (retained(), used());
        let pointer = NonNull::new(surface.as_mut_ptr()).ok_or("no surface")?;
        let lent = Lent::new(pointer, length, true);
        let target = GuestTarget::new(&layout, &lent)?;
        let written = frame.write_to_guest(&target).map_err(|e| e.to_string())?;
        if width % 2 == 1 || height % 2 == 1 {
            assert!(
                matches!(&written, DeviceWrite::Declined(why) if why.contains("odd size")),
                "{name}: {written:?}"
            );
            continue;
        }
        assert_eq!(written, DeviceWrite::Written, "{name}");
        assert_eq!(
            (retained(), used()),
            before,
            "{name}: a reference or use outlived the write"
        );
        let transferred = with_surface_bytes(&surface, length, true, |bytes| bytes.to_vec())?;
        let (copied_samples, copied_rest) = split_samples(&layout, &copied);
        let (samples, rest) = split_samples(&layout, &transferred);
        let differing = (samples.iter().zip(&copied_samples))
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(differing, 0, "{name}: samples unlike the CPU's copy");
        assert_eq!(
            rest, copied_rest,
            "{name}: bytes outside the samples written"
        );
    }
    Ok(())
}

/// How long a frame's write takes each way, and how much of the writing
/// thread's CPU, at 1080p and 2160p, 8 and 10 bits: the CPU's copy, the
/// writer (a pixel buffer made over the surface and a transfer), and a
/// transfer into a pixel buffer kept from one frame to the next, which the
/// writer never keeps (module docs). Ignored: a measurement, not a check.
/// Run it alone, under the Mac's GPU lock:
/// `cargo test -p reims-vgpu-video-videotoolbox --release --lib -- --ignored --nocapture how_long`
#[test]
#[ignore = "a measurement; run it alone"]
fn how_long_a_write_takes() -> Result<(), String> {
    use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
    use std::time::Duration;

    use reims_vgpu_video_core::backend::DecodedFrame as _;

    use crate::macos::decoder::Frame;

    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock: u32) -> u64;
    }
    /// `CLOCK_THREAD_CPUTIME_ID`.
    const THREAD_CPU: u32 = 16;
    // SAFETY: a clock read, no pointer.
    let cpu = || Duration::from_nanos(unsafe { clock_gettime_nsec_np(THREAD_CPU) });
    let rounds = 60;
    let transfer = std::sync::Arc::new(PixelTransfer::default());
    for (format, width, height) in [
        (NV12, 1920, 1080),
        (PixelFormat::P010VideoRange, 1920, 1080),
        (NV12, 3840, 2160),
        (PixelFormat::P010VideoRange, 3840, 2160),
    ] {
        let (layout, length) = guest_layout(format, width, height);
        let frame = Frame::new(1, pattern(format, width, height)?, format, None, &transfer)
            .map_err(|e| e.to_string())?;
        let mut copied = vec![0u8; length];
        let surface = ordinary_surface(&layout, length)?;
        let pointer = NonNull::new(surface.as_mut_ptr()).ok_or("no surface")?;
        let lent = Lent::new(pointer, length, true);
        let target = GuestTarget::new(&layout, &lent)?;
        let mut kept =
            IoSurface::of(lent.io_surface().map_err(|e| e.to_string())?).pixel_buffer()?;
        let measure = |what: &str, f: &mut dyn FnMut() -> Result<(), String>| {
            f()?;
            let (wall, thread) = (StdRuntime.now(), cpu());
            for _ in 0..rounds {
                f()?;
            }
            let (wall, thread) = (
                StdRuntime.elapsed_since(wall) / rounds,
                (cpu() - thread) / rounds,
            );
            eprintln!(
                "{} {width}x{height} {what}: {:.2} ms, {:.2} ms of the thread's CPU",
                format.fourcc(),
                wall.as_secs_f64() * 1e3,
                thread.as_secs_f64() * 1e3
            );
            Ok::<(), String>(())
        };
        measure("the CPU's copy", &mut || {
            let mut planes = layout.split(&mut copied).map_err(|e| e.to_string())?;
            (frame.read_back().map_err(|e| e.to_string())?)
                .copy_to(&mut planes)
                .map_err(|e| e.to_string())
        })?;
        measure("the writer", &mut || match frame
            .write_to_guest(&target)
            .map_err(|e| e.to_string())?
        {
            DeviceWrite::Written => Ok(()),
            DeviceWrite::Declined(why) => Err(why),
        })?;
        measure("a transfer into a kept pixel buffer", &mut || {
            transfer
                .transfer(frame.image(), &mut kept)
                .map_err(|status| status.to_string())
        })?;
    }
    Ok(())
}
