//! CoreMedia: times, format descriptions, block buffers and sample buffers
//! (`CoreMedia/CMTime.h`, `CMFormatDescription.h`, `CMBlockBuffer.h`,
//! `CMSampleBuffer.h`).

#![expect(non_upper_case_globals, reason = "the SDK's own names")]

use std::ffi::c_void;

use super::core_foundation::{
    Boolean, CFAllocatorRef, CFArrayRef, CFDictionaryRef, CFIndex, CFPropertyListRef, CFStringRef,
    OSStatus,
};
use super::{constants, declare};

/// `CMVideoCodecType`: a `FourCharCode`.
pub(crate) type CMVideoCodecType = u32;
/// `CMItemCount`: a `CFIndex`.
pub(crate) type CMItemCount = CFIndex;
/// `CMBlockBufferFlags`: `uint32_t`.
pub(crate) type CMBlockBufferFlags = u32;
/// `CMFormatDescriptionRef`.
pub(crate) type CMFormatDescriptionRef = *const c_void;
/// `CMVideoFormatDescriptionRef`.
pub(crate) type CMVideoFormatDescriptionRef = *const c_void;
/// `CMBlockBufferRef`.
pub(crate) type CMBlockBufferRef = *mut c_void;
/// `CMSampleBufferRef`.
pub(crate) type CMSampleBufferRef = *mut c_void;

/// `CMTime`, the same 24 bytes as core's `MediaTime`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CMTime {
    /// `value`.
    pub(crate) value: i64,
    /// `timescale`.
    pub(crate) timescale: i32,
    /// `flags`.
    pub(crate) flags: u32,
    /// `epoch`.
    pub(crate) epoch: i64,
}

/// `CMSampleTimingInfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CMSampleTimingInfo {
    /// `duration`.
    pub(crate) duration: CMTime,
    /// `presentationTimeStamp`.
    pub(crate) presentation_time_stamp: CMTime,
    /// `decodeTimeStamp`.
    pub(crate) decode_time_stamp: CMTime,
}

/// `CMBlockBufferCustomBlockSource`, only ever passed as NULL.
#[repr(C)]
pub(crate) struct CMBlockBufferCustomBlockSource {
    _opaque: [u8; 0],
}

declare! {
    #[link(name = "CoreMedia", kind = "framework")]
    functions {
        fn CMVideoFormatDescriptionCreate(
            allocator: CFAllocatorRef,
            codec_type: CMVideoCodecType,
            width: i32,
            height: i32,
            extensions: CFDictionaryRef,
            format_description_out: *mut CMVideoFormatDescriptionRef,
        ) -> OSStatus;
        fn CMFormatDescriptionGetExtension(
            desc: CMFormatDescriptionRef,
            extension_key: CFStringRef,
        ) -> CFPropertyListRef;
        fn CMBlockBufferCreateWithMemoryBlock(
            structure_allocator: CFAllocatorRef,
            memory_block: *mut c_void,
            block_length: usize,
            block_allocator: CFAllocatorRef,
            custom_block_source: *const CMBlockBufferCustomBlockSource,
            offset_to_data: usize,
            data_length: usize,
            flags: CMBlockBufferFlags,
            block_buffer_out: *mut CMBlockBufferRef,
        ) -> OSStatus;
        fn CMBlockBufferReplaceDataBytes(
            source_bytes: *const c_void,
            destination_buffer: CMBlockBufferRef,
            offset_into_destination: usize,
            data_length: usize,
        ) -> OSStatus;
        fn CMBlockBufferGetDataLength(the_buffer: CMBlockBufferRef) -> usize;
        fn CMBlockBufferCopyDataBytes(
            the_source_buffer: CMBlockBufferRef,
            offset_to_data: usize,
            data_length: usize,
            destination: *mut c_void,
        ) -> OSStatus;
        fn CMSampleBufferCreateReady(
            allocator: CFAllocatorRef,
            data_buffer: CMBlockBufferRef,
            format_description: CMFormatDescriptionRef,
            num_samples: CMItemCount,
            num_sample_timing_entries: CMItemCount,
            sample_timing_array: *const CMSampleTimingInfo,
            num_sample_size_entries: CMItemCount,
            sample_size_array: *const usize,
            sample_buffer_out: *mut CMSampleBufferRef,
        ) -> OSStatus;
        fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
        fn CMSampleBufferGetFormatDescription(sbuf: CMSampleBufferRef) -> CMFormatDescriptionRef;
        fn CMSampleBufferGetSampleAttachmentsArray(
            sbuf: CMSampleBufferRef,
            create_if_necessary: Boolean,
        ) -> CFArrayRef;
        fn CMSampleBufferGetPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
        fn CMSampleBufferGetDecodeTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
        fn CMSampleBufferGetDuration(sbuf: CMSampleBufferRef) -> CMTime;
    }
    statics {
        static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFStringRef;
        static kCMSampleAttachmentKey_NotSync: CFStringRef;
        static kCMSampleAttachmentKey_DependsOnOthers: CFStringRef;
        static kCMSampleAttachmentKey_IsDependedOnByOthers: CFStringRef;
    }
}

constants! {
    /// `kCMBlockBufferAssureMemoryNowFlag`.
    pub(crate) const kCMBlockBufferAssureMemoryNowFlag: CMBlockBufferFlags = 1;
    /// `kCMVideoCodecType_H264`: `avc1`.
    pub(crate) const kCMVideoCodecType_H264: CMVideoCodecType = 0x6176_6331;
    /// `kCMVideoCodecType_HEVC`: `hvc1`.
    pub(crate) const kCMVideoCodecType_HEVC: CMVideoCodecType = 0x6876_6331;
    /// `kCMVideoCodecType_AV1`: `av01`.
    pub(crate) const kCMVideoCodecType_AV1: CMVideoCodecType = 0x6176_3031;
    /// `kCMVideoCodecType_VP9`: `vp09`.
    pub(crate) const kCMVideoCodecType_VP9: CMVideoCodecType = 0x7670_3039;
}
