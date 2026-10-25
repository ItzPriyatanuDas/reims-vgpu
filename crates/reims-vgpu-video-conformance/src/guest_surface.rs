//! The stand-in guest's buffers on macOS as IOSurfaces laid out as a
//! guest's are, so that the VideoToolbox backend's zero copy writes them as
//! it writes a real guest's (core's `DeviceMemory::io_surface`).
//!
//! reims-vgpu lends a guest's buffer as the IOSurface it made over the
//! guest's pages, at the planes the guest gave. [`GuestIoSurface`] is an
//! IOSurface of its own memory at the same planes, made by IOSurface from a
//! plane list, never over pages of client memory: the VideoToolbox backend
//! transfers into it on the Mac's hardware, and a transfer into an
//! IOSurface over pages read-only for its clients panicked the Mac it ran
//! on (an M3 Max), so the stand-in makes none over client memory at all. The CPU reaches its bytes
//! at its base address between `IOSurfaceLock` and `IOSurfaceUnlock`
//! ([`GuestIoSurface::lock`]), so a read after a device's write sees it.

use std::ffi::c_void;
use std::ptr::NonNull;

use reims_vgpu_video_core::backend::PixelFormat;
use reims_vgpu_video_core::surfaces::SurfaceLayout;

type CFTypeRef = *const c_void;
type CFIndex = isize;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFNumberCreate(allocator: CFTypeRef, the_type: CFIndex, value: *const c_void) -> CFTypeRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *mut CFTypeRef,
        values: *mut CFTypeRef,
        count: CFIndex,
        key_call_backs: *const c_void,
        value_call_backs: *const c_void,
    ) -> CFTypeRef;
    fn CFArrayCreate(
        allocator: CFTypeRef,
        values: *mut CFTypeRef,
        count: CFIndex,
        call_backs: *const c_void,
    ) -> CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFTypeArrayCallBacks: c_void;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreate(properties: CFTypeRef) -> *mut c_void;
    fn IOSurfaceGetBaseAddress(buffer: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetAllocSize(buffer: *mut c_void) -> usize;
    fn IOSurfaceLock(buffer: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(buffer: *mut c_void, options: u32, seed: *mut u32) -> i32;
    static kIOSurfaceWidth: CFTypeRef;
    static kIOSurfaceHeight: CFTypeRef;
    static kIOSurfacePixelFormat: CFTypeRef;
    static kIOSurfaceAllocSize: CFTypeRef;
    static kIOSurfacePlaneInfo: CFTypeRef;
    static kIOSurfacePlaneWidth: CFTypeRef;
    static kIOSurfacePlaneHeight: CFTypeRef;
    static kIOSurfacePlaneBytesPerRow: CFTypeRef;
    static kIOSurfacePlaneBytesPerElement: CFTypeRef;
    static kIOSurfacePlaneElementWidth: CFTypeRef;
    static kIOSurfacePlaneElementHeight: CFTypeRef;
    static kIOSurfacePlaneOffset: CFTypeRef;
    static kIOSurfacePlaneSize: CFTypeRef;
}

/// `kCFNumberSInt64Type`.
const SINT64: CFIndex = 4;

/// One CoreFoundation reference, released on drop.
struct Cf(NonNull<c_void>);

impl Cf {
    /// Takes a Create call's result.
    fn take(made: CFTypeRef) -> Result<Self, String> {
        NonNull::new(made.cast_mut())
            .map(Self)
            .ok_or_else(|| "CoreFoundation made nothing".to_owned())
    }

    fn as_ptr(&self) -> CFTypeRef {
        self.0.as_ptr()
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: one reference this owns, given back once.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

fn number(value: usize) -> Result<Cf, String> {
    let value = i64::try_from(value).map_err(|_| "a number past i64")?;
    // SAFETY: a live i64 of the type named.
    Cf::take(unsafe { CFNumberCreate(std::ptr::null(), SINT64, (&raw const value).cast()) })
}

fn dictionary(entries: &[(CFTypeRef, &Cf)]) -> Result<Cf, String> {
    let mut keys: Vec<CFTypeRef> = entries.iter().map(|(key, _)| *key).collect();
    let mut values: Vec<CFTypeRef> = entries.iter().map(|(_, value)| value.as_ptr()).collect();
    // SAFETY: the arrays hold live objects for the count given; the CFType
    // callbacks retain each.
    Cf::take(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            entries.len() as CFIndex,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    })
}

/// A buffer's bytes as an IOSurface of its own memory laid out as the
/// guest's buffer (module docs).
pub(crate) struct GuestIoSurface {
    surface: NonNull<c_void>,
    len: usize,
}

// SAFETY: an IOSurfaceRef is a thread-safe CoreFoundation object; its bytes
// are reached only under its lock, through `&mut self` or a lock.
unsafe impl Send for GuestIoSurface {}
// SAFETY: as above.
unsafe impl Sync for GuestIoSurface {}

impl GuestIoSurface {
    /// An IOSurface of `len` bytes laid out as `layout`, every byte 0;
    /// `Err` says why there is none.
    pub(crate) fn new(layout: &SurfaceLayout, len: usize) -> Result<Self, String> {
        let format = PixelFormat::from_fourcc(layout.pixel_format)
            .ok_or_else(|| format!("'{}', which no decoder writes", layout.pixel_format))?;
        let sample = format.bytes_per_sample();
        let mut planes = Vec::new();
        for (index, plane) in layout.planes.iter().enumerate() {
            let values = [
                number(plane.width as usize)?,
                number(plane.height as usize)?,
                number(plane.bytes_per_row)?,
                number(if index == 0 { sample } else { 2 * sample })?,
                number(1)?,
                number(1)?,
                number(plane.offset)?,
                number(plane.bytes_per_row * plane.height as usize)?,
            ];
            // SAFETY: reading IOSurface's constant keys.
            let keys = unsafe {
                [
                    kIOSurfacePlaneWidth,
                    kIOSurfacePlaneHeight,
                    kIOSurfacePlaneBytesPerRow,
                    kIOSurfacePlaneBytesPerElement,
                    kIOSurfacePlaneElementWidth,
                    kIOSurfacePlaneElementHeight,
                    kIOSurfacePlaneOffset,
                    kIOSurfacePlaneSize,
                ]
            };
            let entries: Vec<_> = keys.into_iter().zip(&values).collect();
            planes.push(dictionary(&entries)?);
        }
        let mut list: Vec<CFTypeRef> = planes.iter().map(Cf::as_ptr).collect();
        // SAFETY: live dictionaries for the count given, each retained.
        let list = Cf::take(unsafe {
            CFArrayCreate(
                std::ptr::null(),
                list.as_mut_ptr(),
                list.len() as CFIndex,
                &raw const kCFTypeArrayCallBacks,
            )
        })?;
        let values = [
            number(layout.width as usize)?,
            number(layout.height as usize)?,
            number(layout.pixel_format.0 as usize)?,
            number(len)?,
        ];
        // SAFETY: reading IOSurface's constant keys.
        let keys = unsafe {
            [
                kIOSurfaceWidth,
                kIOSurfaceHeight,
                kIOSurfacePixelFormat,
                kIOSurfaceAllocSize,
            ]
        };
        let mut entries: Vec<_> = keys.into_iter().zip(&values).collect();
        // SAFETY: as above.
        entries.push((unsafe { kIOSurfacePlaneInfo }, &list));
        let properties = dictionary(&entries)?;
        // SAFETY: a live dictionary of IOSurface's keys, none of client
        // memory: the surface is of its own memory.
        let made = unsafe { IOSurfaceCreate(properties.as_ptr()) };
        let surface = NonNull::new(made).ok_or("IOSurfaceCreate made none")?;
        let made = Self { surface, len };
        // SAFETY: a live surface.
        if unsafe { IOSurfaceGetAllocSize(surface.as_ptr()) } < len {
            return Err("IOSurfaceCreate made a smaller surface".to_owned());
        }
        made.lock()?.bytes().fill(0);
        Ok(made)
    }

    /// The surface, for a host to lend as its IOSurface.
    pub(crate) fn as_ptr(&self) -> NonNull<c_void> {
        self.surface
    }

    /// The surface locked for the CPU, its bytes reached through the
    /// result until it drops, which holds a reference of its own.
    pub(crate) fn lock(&self) -> Result<Locked, String> {
        // SAFETY: a live surface; a NULL seed is allowed.
        let status = unsafe { IOSurfaceLock(self.surface.as_ptr(), 0, std::ptr::null_mut()) };
        if status != 0 {
            return Err(format!("IOSurfaceLock answered {status:#x}"));
        }
        // SAFETY: a live surface; the reference is the result's, given back
        // as it drops, with the lock.
        unsafe { CFRetain(self.surface.as_ptr().cast_const()) };
        let locked = Locked {
            surface: self.surface,
            len: self.len,
        };
        // SAFETY: a live surface.
        if unsafe { IOSurfaceGetBaseAddress(self.surface.as_ptr()) }.is_null() {
            return Err("an IOSurface with no base address".to_owned());
        }
        Ok(locked)
    }
}

impl Drop for GuestIoSurface {
    fn drop(&mut self) {
        // SAFETY: the reference IOSurfaceCreate gave, released once.
        unsafe { CFRelease(self.surface.as_ptr().cast_const()) };
    }
}

/// A [`GuestIoSurface`] locked for the CPU (`IOSurfaceLock`), unlocked as
/// it drops, with a reference of its own to the surface.
pub(crate) struct Locked {
    surface: NonNull<c_void>,
    len: usize,
}

// SAFETY: as for the surface; its bytes are reached through `&mut self`.
unsafe impl Send for Locked {}

impl Locked {
    /// The surface's bytes from its base address, its buffer's `len`.
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the locked surface's own memory, at least `len` bytes of
        // its allocation from its base address (checked at its making),
        // reached only through this lock while it lives.
        unsafe {
            std::slice::from_raw_parts_mut(
                IOSurfaceGetBaseAddress(self.surface.as_ptr()).cast::<u8>(),
                self.len,
            )
        }
    }

    /// The same bytes, to read.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        // SAFETY: as for `bytes`.
        unsafe {
            std::slice::from_raw_parts(
                IOSurfaceGetBaseAddress(self.surface.as_ptr())
                    .cast::<u8>()
                    .cast_const(),
                self.len,
            )
        }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: locked by `GuestIoSurface::lock` with no options, and the
        // reference it took released once.
        unsafe {
            IOSurfaceUnlock(self.surface.as_ptr(), 0, std::ptr::null_mut());
            CFRelease(self.surface.as_ptr().cast_const());
        }
    }
}
