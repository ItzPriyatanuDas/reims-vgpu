//! IOSurface: the largest surface it promises, a surface's own answers of its
//! memory and planes, which the zero copy reads of a guest buffer before a
//! transfer writes it, and surfaces of their own memory for the tests
//! (`IOSurface/IOSurfaceRef.h`).

#![expect(non_upper_case_globals, reason = "the SDK's own names")]

use std::ffi::c_void;

use super::core_foundation::{CFDictionaryRef, CFStringRef, OSType};
use super::core_video::IOSurfaceRef;
use super::mach::kern_return_t;
use super::{constants, declare};

/// `IOSurfaceLockOptions`: a `uint32_t` enum.
pub(crate) type IOSurfaceLockOptions = u32;

declare! {
    #[link(name = "IOSurface", kind = "framework")]
    functions {
        fn IOSurfaceGetPropertyMaximum(property: CFStringRef) -> usize;
        fn IOSurfaceCreate(properties: CFDictionaryRef) -> IOSurfaceRef;
        fn IOSurfaceGetBaseAddress(buffer: IOSurfaceRef) -> *mut c_void;
        fn IOSurfaceGetAllocSize(buffer: IOSurfaceRef) -> usize;
        fn IOSurfaceGetPixelFormat(buffer: IOSurfaceRef) -> OSType;
        fn IOSurfaceGetWidth(buffer: IOSurfaceRef) -> usize;
        fn IOSurfaceGetHeight(buffer: IOSurfaceRef) -> usize;
        fn IOSurfaceGetPlaneCount(buffer: IOSurfaceRef) -> usize;
        fn IOSurfaceGetBaseAddressOfPlane(buffer: IOSurfaceRef, plane_index: usize) -> *mut c_void;
        fn IOSurfaceGetBytesPerRowOfPlane(buffer: IOSurfaceRef, plane_index: usize) -> usize;
        fn IOSurfaceGetWidthOfPlane(buffer: IOSurfaceRef, plane_index: usize) -> usize;
        fn IOSurfaceGetHeightOfPlane(buffer: IOSurfaceRef, plane_index: usize) -> usize;
        fn IOSurfaceGetUseCount(buffer: IOSurfaceRef) -> i32;
        fn IOSurfaceLock(
            buffer: IOSurfaceRef,
            options: IOSurfaceLockOptions,
            seed: *mut u32,
        ) -> kern_return_t;
        fn IOSurfaceUnlock(
            buffer: IOSurfaceRef,
            options: IOSurfaceLockOptions,
            seed: *mut u32,
        ) -> kern_return_t;
    }
    statics {
        static kIOSurfaceWidth: CFStringRef;
        static kIOSurfaceHeight: CFStringRef;
        static kIOSurfaceBytesPerRow: CFStringRef;
        static kIOSurfacePixelFormat: CFStringRef;
        static kIOSurfaceAllocSize: CFStringRef;
        static kIOSurfacePlaneInfo: CFStringRef;
        static kIOSurfacePlaneWidth: CFStringRef;
        static kIOSurfacePlaneHeight: CFStringRef;
        static kIOSurfacePlaneBytesPerRow: CFStringRef;
        static kIOSurfacePlaneBytesPerElement: CFStringRef;
        static kIOSurfacePlaneElementWidth: CFStringRef;
        static kIOSurfacePlaneElementHeight: CFStringRef;
        static kIOSurfacePlaneOffset: CFStringRef;
        static kIOSurfacePlaneSize: CFStringRef;
    }
}

constants! {
    /// `kIOSurfaceLockReadOnly`.
    pub(crate) const kIOSurfaceLockReadOnly: IOSurfaceLockOptions = 1;
}
