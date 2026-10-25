//! CoreVideo: pixel buffers, their planes, pools and attachments
//! (`CoreVideo/CVPixelBuffer.h`, `CVPixelBufferPool.h`, `CVBuffer.h`,
//! `CVImageBuffer.h`, `CVPixelBufferIOSurface.h`).

#![expect(non_upper_case_globals, reason = "the SDK's own names")]

use std::ffi::c_void;

use super::core_foundation::{
    Boolean, CFAllocatorRef, CFDictionaryRef, CFStringRef, CFTypeRef, OSType,
};
use super::{constants, declare};

/// `CVReturn`: `int32_t`.
pub(crate) type CVReturn = i32;
/// `CVPixelBufferLockFlags`: `CVOptionFlags`, a `uint64_t`.
pub(crate) type CVPixelBufferLockFlags = u64;
/// `CVAttachmentMode`: `uint32_t`.
pub(crate) type CVAttachmentMode = u32;
/// `CVBufferRef`.
pub(crate) type CVBufferRef = *mut c_void;
/// `CVImageBufferRef`: a `CVBufferRef`.
pub(crate) type CVImageBufferRef = *mut c_void;
/// `CVPixelBufferRef`: a `CVImageBufferRef`.
pub(crate) type CVPixelBufferRef = *mut c_void;
/// `CVPixelBufferPoolRef`.
pub(crate) type CVPixelBufferPoolRef = *mut c_void;
/// `IOSurfaceRef`.
pub(crate) type IOSurfaceRef = *mut c_void;

declare! {
    #[link(name = "CoreVideo", kind = "framework")]
    functions {
        fn CVPixelBufferCreate(
            allocator: CFAllocatorRef,
            width: usize,
            height: usize,
            pixel_format_type: OSType,
            pixel_buffer_attributes: CFDictionaryRef,
            pixel_buffer_out: *mut CVPixelBufferRef,
        ) -> CVReturn;
        fn CVPixelBufferPoolCreatePixelBuffer(
            allocator: CFAllocatorRef,
            pixel_buffer_pool: CVPixelBufferPoolRef,
            pixel_buffer_out: *mut CVPixelBufferRef,
        ) -> CVReturn;
        fn CVPixelBufferLockBaseAddress(
            pixel_buffer: CVPixelBufferRef,
            lock_flags: CVPixelBufferLockFlags,
        ) -> CVReturn;
        fn CVPixelBufferUnlockBaseAddress(
            pixel_buffer: CVPixelBufferRef,
            unlock_flags: CVPixelBufferLockFlags,
        ) -> CVReturn;
        fn CVPixelBufferGetPixelFormatType(pixel_buffer: CVPixelBufferRef) -> OSType;
        fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferIsPlanar(pixel_buffer: CVPixelBufferRef) -> Boolean;
        fn CVPixelBufferGetPlaneCount(pixel_buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut c_void;
        fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetBaseAddressOfPlane(
            pixel_buffer: CVPixelBufferRef,
            plane_index: usize,
        ) -> *mut c_void;
        fn CVPixelBufferGetBytesPerRowOfPlane(
            pixel_buffer: CVPixelBufferRef,
            plane_index: usize,
        ) -> usize;
        fn CVPixelBufferGetHeightOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
        fn CVPixelBufferCreateWithIOSurface(
            allocator: CFAllocatorRef,
            surface: IOSurfaceRef,
            pixel_buffer_attributes: CFDictionaryRef,
            pixel_buffer_out: *mut CVPixelBufferRef,
        ) -> CVReturn;
        fn CVBufferSetAttachment(
            buffer: CVBufferRef,
            key: CFStringRef,
            value: CFTypeRef,
            attachment_mode: CVAttachmentMode,
        );
        fn CVBufferRemoveAllAttachments(buffer: CVBufferRef);
    }
    statics {
        static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
        static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
        static kCVImageBufferYCbCrMatrixKey: CFStringRef;
        static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFStringRef;
        static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
        static kCVImageBufferCleanApertureKey: CFStringRef;
        static kCVImageBufferCleanApertureWidthKey: CFStringRef;
        static kCVImageBufferCleanApertureHeightKey: CFStringRef;
        static kCVImageBufferCleanApertureHorizontalOffsetKey: CFStringRef;
        static kCVImageBufferCleanApertureVerticalOffsetKey: CFStringRef;
        static kCVImageBufferChromaLocationTopFieldKey: CFStringRef;
        static kCVImageBufferChromaLocationBottomFieldKey: CFStringRef;
        static kCVImageBufferChromaLocation_Left: CFStringRef;
        static kCVImageBufferChromaLocation_Center: CFStringRef;
        static kCVImageBufferChromaLocation_TopLeft: CFStringRef;
        static kCVImageBufferChromaLocation_Top: CFStringRef;
        static kCVImageBufferChromaLocation_BottomLeft: CFStringRef;
        static kCVImageBufferChromaLocation_Bottom: CFStringRef;
    }
}

constants! {
    /// `kCVReturnSuccess`.
    pub(crate) const kCVReturnSuccess: CVReturn = 0;
    /// `kCVPixelBufferLock_ReadOnly`.
    pub(crate) const kCVPixelBufferLock_ReadOnly: CVPixelBufferLockFlags = 1;
    /// `kCVAttachmentMode_ShouldPropagate`.
    pub(crate) const kCVAttachmentMode_ShouldPropagate: CVAttachmentMode = 1;
    /// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`: `420v`.
    pub(crate) const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: OSType = 0x3432_3076;
    /// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`: `420f`.
    pub(crate) const kCVPixelFormatType_420YpCbCr8BiPlanarFullRange: OSType = 0x3432_3066;
    /// `kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange`: `x420`.
    pub(crate) const kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange: OSType = 0x7834_3230;
    /// `kCVPixelFormatType_420YpCbCr10BiPlanarFullRange`: `xf20`.
    pub(crate) const kCVPixelFormatType_420YpCbCr10BiPlanarFullRange: OSType = 0x7866_3230;
    /// `kCVPixelFormatType_32BGRA`: `BGRA`.
    pub(crate) const kCVPixelFormatType_32BGRA: OSType = 0x4247_5241;
    /// `kCVPixelFormatType_32ARGB`: 0x20.
    pub(crate) const kCVPixelFormatType_32ARGB: OSType = 0x20;
    /// `kCVPixelFormatType_422YpCbCr8_yuvs`: `yuvs`.
    pub(crate) const kCVPixelFormatType_422YpCbCr8_yuvs: OSType = 0x7975_7673;
    /// `kCVPixelFormatType_422YpCbCr8`: `2vuy`.
    pub(crate) const kCVPixelFormatType_422YpCbCr8: OSType = 0x3276_7579;
}
