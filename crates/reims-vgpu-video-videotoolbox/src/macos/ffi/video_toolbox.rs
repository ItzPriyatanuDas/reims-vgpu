//! VideoToolbox: decompression, compression, pixel transfer and rotation
//! sessions, their properties, and the encoder list
//! (`VideoToolbox/VTDecompressionSession.h`, `VTCompressionSession.h`,
//! `VTSession.h`, `VTPixelTransferSession.h`, `VTPixelRotationSession.h`,
//! `VTVideoEncoderList.h`, `VTUtilities.h`, `VTErrors.h`, and the property
//! headers).

#![expect(non_upper_case_globals, reason = "the SDK's own names")]

use std::ffi::c_void;

use super::core_foundation::{
    Boolean, CFAllocatorRef, CFArrayRef, CFDictionaryRef, CFStringRef, CFTypeRef, OSStatus,
};
use super::core_media::{
    CMFormatDescriptionRef, CMSampleBufferRef, CMTime, CMVideoCodecType,
    CMVideoFormatDescriptionRef,
};
use super::core_video::{CVImageBufferRef, CVPixelBufferPoolRef, CVPixelBufferRef};
use super::{constants, declare};

/// `VTSessionRef`: any of the sessions, as a `CFTypeRef`.
pub(crate) type VTSessionRef = CFTypeRef;
/// `VTDecompressionSessionRef`.
pub(crate) type VTDecompressionSessionRef = *mut c_void;
/// `VTCompressionSessionRef`.
pub(crate) type VTCompressionSessionRef = *mut c_void;
/// `VTPixelTransferSessionRef`.
pub(crate) type VTPixelTransferSessionRef = *mut c_void;
/// `VTPixelRotationSessionRef`.
pub(crate) type VTPixelRotationSessionRef = *mut c_void;
/// `VTDecodeFrameFlags`: a `uint32_t` set.
pub(crate) type VTDecodeFrameFlags = u32;
/// `VTDecodeInfoFlags`: a `uint32_t` set.
pub(crate) type VTDecodeInfoFlags = u32;
/// `VTEncodeInfoFlags`: a `uint32_t` set.
pub(crate) type VTEncodeInfoFlags = u32;

/// `VTDecompressionOutputCallback`.
pub(crate) type VTDecompressionOutputCallback = Option<
    unsafe extern "C" fn(
        decompression_output_ref_con: *mut c_void,
        source_frame_ref_con: *mut c_void,
        status: OSStatus,
        info_flags: VTDecodeInfoFlags,
        image_buffer: CVImageBufferRef,
        presentation_time_stamp: CMTime,
        presentation_duration: CMTime,
    ),
>;

/// `VTCompressionOutputCallback`.
pub(crate) type VTCompressionOutputCallback = Option<
    unsafe extern "C" fn(
        output_callback_ref_con: *mut c_void,
        source_frame_ref_con: *mut c_void,
        status: OSStatus,
        info_flags: VTEncodeInfoFlags,
        sample_buffer: CMSampleBufferRef,
    ),
>;

/// `VTDecompressionOutputCallbackRecord`.
#[repr(C)]
pub(crate) struct VTDecompressionOutputCallbackRecord {
    /// `decompressionOutputCallback`.
    pub(crate) callback: VTDecompressionOutputCallback,
    /// `decompressionOutputRefCon`.
    pub(crate) ref_con: *mut c_void,
}

declare! {
    #[link(name = "VideoToolbox", kind = "framework")]
    functions {
        fn VTIsHardwareDecodeSupported(codec_type: CMVideoCodecType) -> Boolean;
        fn VTRegisterSupplementalVideoDecoderIfAvailable(codec_type: CMVideoCodecType);
        fn VTSessionSetProperty(
            session: VTSessionRef,
            property_key: CFStringRef,
            property_value: CFTypeRef,
        ) -> OSStatus;
        fn VTDecompressionSessionCreate(
            allocator: CFAllocatorRef,
            video_format_description: CMVideoFormatDescriptionRef,
            video_decoder_specification: CFDictionaryRef,
            destination_image_buffer_attributes: CFDictionaryRef,
            output_callback: *const VTDecompressionOutputCallbackRecord,
            decompression_session_out: *mut VTDecompressionSessionRef,
        ) -> OSStatus;
        fn VTDecompressionSessionDecodeFrame(
            session: VTDecompressionSessionRef,
            sample_buffer: CMSampleBufferRef,
            decode_flags: VTDecodeFrameFlags,
            source_frame_ref_con: *mut c_void,
            info_flags_out: *mut VTDecodeInfoFlags,
        ) -> OSStatus;
        fn VTDecompressionSessionFinishDelayedFrames(session: VTDecompressionSessionRef) -> OSStatus;
        fn VTDecompressionSessionWaitForAsynchronousFrames(
            session: VTDecompressionSessionRef,
        ) -> OSStatus;
        fn VTDecompressionSessionCanAcceptFormatDescription(
            session: VTDecompressionSessionRef,
            new_format_desc: CMFormatDescriptionRef,
        ) -> Boolean;
        fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
        fn VTCompressionSessionCreate(
            allocator: CFAllocatorRef,
            width: i32,
            height: i32,
            codec_type: CMVideoCodecType,
            encoder_specification: CFDictionaryRef,
            source_image_buffer_attributes: CFDictionaryRef,
            compressed_data_allocator: CFAllocatorRef,
            output_callback: VTCompressionOutputCallback,
            output_callback_ref_con: *mut c_void,
            compression_session_out: *mut VTCompressionSessionRef,
        ) -> OSStatus;
        fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
        fn VTCompressionSessionGetPixelBufferPool(
            session: VTCompressionSessionRef,
        ) -> CVPixelBufferPoolRef;
        fn VTCompressionSessionEncodeFrame(
            session: VTCompressionSessionRef,
            image_buffer: CVImageBufferRef,
            presentation_time_stamp: CMTime,
            duration: CMTime,
            frame_properties: CFDictionaryRef,
            source_frame_refcon: *mut c_void,
            info_flags_out: *mut VTEncodeInfoFlags,
        ) -> OSStatus;
        fn VTCompressionSessionCompleteFrames(
            session: VTCompressionSessionRef,
            complete_until_presentation_time_stamp: CMTime,
        ) -> OSStatus;
        fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
        fn VTCopyVideoEncoderList(
            options: CFDictionaryRef,
            list_of_video_encoders_out: *mut CFArrayRef,
        ) -> OSStatus;
        fn VTCopySupportedPropertyDictionaryForEncoder(
            width: i32,
            height: i32,
            codec_type: CMVideoCodecType,
            encoder_specification: CFDictionaryRef,
            encoder_id_out: *mut CFStringRef,
            supported_properties_out: *mut CFDictionaryRef,
        ) -> OSStatus;
        fn VTPixelTransferSessionCreate(
            allocator: CFAllocatorRef,
            pixel_transfer_session_out: *mut VTPixelTransferSessionRef,
        ) -> OSStatus;
        fn VTPixelTransferSessionTransferImage(
            session: VTPixelTransferSessionRef,
            source_buffer: CVPixelBufferRef,
            destination_buffer: CVPixelBufferRef,
        ) -> OSStatus;
        fn VTPixelTransferSessionInvalidate(session: VTPixelTransferSessionRef);
        fn VTPixelRotationSessionCreate(
            allocator: CFAllocatorRef,
            pixel_rotation_session_out: *mut VTPixelRotationSessionRef,
        ) -> OSStatus;
        fn VTPixelRotationSessionRotateImage(
            session: VTPixelRotationSessionRef,
            source_buffer: CVPixelBufferRef,
            destination_buffer: CVPixelBufferRef,
        ) -> OSStatus;
        fn VTPixelRotationSessionInvalidate(session: VTPixelRotationSessionRef);
    }
    statics {
        static kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: CFStringRef;
        static kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: CFStringRef;
        static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
        static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
        static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
        static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
        static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
        static kVTCompressionPropertyKey_RealTime: CFStringRef;
        static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
        static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
        static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
        static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
        static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
        static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
        static kVTProfileLevel_HEVC_Main10_AutoLevel: CFStringRef;
        static kVTVideoEncoderList_CodecType: CFStringRef;
        static kVTVideoEncoderList_IsHardwareAccelerated: CFStringRef;
        static kVTVideoEncoderList_SupportsFrameReordering: CFStringRef;
        static kVTPixelTransferPropertyKey_ScalingMode: CFStringRef;
        static kVTScalingMode_CropSourceToCleanAperture: CFStringRef;
        static kVTPixelTransferPropertyKey_DestinationYCbCrMatrix: CFStringRef;
        static kVTPixelTransferPropertyKey_DestinationCleanAperture: CFStringRef;
        static kVTPixelRotationPropertyKey_Rotation: CFStringRef;
        static kVTRotation_0: CFStringRef;
        static kVTRotation_CW90: CFStringRef;
        static kVTPixelRotationPropertyKey_FlipHorizontalOrientation: CFStringRef;
        static kVTPixelRotationPropertyKey_FlipVerticalOrientation: CFStringRef;
    }
}

constants! {
    /// `kVTDecodeInfo_FrameDropped`.
    pub(crate) const kVTDecodeInfo_FrameDropped: VTDecodeInfoFlags = 2;
    /// `kVTDecodeInfo_SkippedLeadingFrameDropped`.
    pub(crate) const kVTDecodeInfo_SkippedLeadingFrameDropped: VTDecodeInfoFlags = 8;
    /// `kVTEncodeInfo_FrameDropped`.
    pub(crate) const kVTEncodeInfo_FrameDropped: VTEncodeInfoFlags = 2;
    /// `kVTPropertyNotSupportedErr`.
    pub(crate) const kVTPropertyNotSupportedErr: OSStatus = -12900;
    /// `kVTParameterErr`.
    pub(crate) const kVTParameterErr: OSStatus = -12902;
    /// `kVTPixelTransferNotSupportedErr`.
    pub(crate) const kVTPixelTransferNotSupportedErr: OSStatus = -12905;
    /// `kVTCouldNotFindVideoDecoderErr`.
    pub(crate) const kVTCouldNotFindVideoDecoderErr: OSStatus = -12906;
    /// `kVTVideoEncoderMalfunctionErr`.
    pub(crate) const kVTVideoEncoderMalfunctionErr: OSStatus = -12912;
    /// `kVTImageRotationNotSupportedErr`.
    pub(crate) const kVTImageRotationNotSupportedErr: OSStatus = -12914;
    /// `kVTVideoDecoderReferenceMissingErr`.
    pub(crate) const kVTVideoDecoderReferenceMissingErr: OSStatus = -17694;
}
