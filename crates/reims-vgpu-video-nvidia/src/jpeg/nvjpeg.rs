//! nvJPEG (libnvjpeg), loaded at run time: the declarations the JPEG
//! decoder uses, and the library.
//!
//! libnvjpeg is a CUDA toolkit library, not a driver one: NVIDIA's driver
//! package does not install it (nvidia-utils 615.71.09 has no such file),
//! a CUDA toolkit or its redistributable `libnvjpeg` archive does. Where
//! the loader finds none, the backend has no JPEG engine. libnvjpeg 13
//! links the CUDA runtime in, so it needs nothing beyond the driver's
//! libcuda (`readelf -d` of libnvjpeg.so.13.2.3.58: libc, libm, libdl,
//! libpthread, librt and libgcc_s, 2026-10-25).
//!
//! The declarations are written from `nvjpeg.h` 13.2.3.58 (CUDA 13.4.3's
//! redistributable libnvjpeg), which no package on the hosts installs, so
//! `crate::layout` cannot check them: each function takes and returns
//! `int`-sized enums, sizes and pointers, and `nvjpegImage_t` is the one
//! struct.

#![allow(
    non_camel_case_types,
    non_snake_case,
    reason = "NVIDIA's names, kept for grepping the header"
)]

use std::ffi::{c_int, c_uint, c_void};
use std::sync::OnceLock;

use crate::cuda::ffi::CUstream;
use crate::dl::{Library, LoadError, functions};

/// `nvjpegStatus_t`: 0 is success.
pub(crate) type nvjpegStatus_t = c_int;
/// `nvjpegHandle_t`: the library's handle, bound to the context current at
/// its creation.
pub(crate) type nvjpegHandle_t = *mut c_void;
/// `nvjpegJpegState_t`: a decoder's state between its phases.
pub(crate) type nvjpegJpegState_t = *mut c_void;
/// `nvjpegJpegStream_t`: a parsed stream.
pub(crate) type nvjpegJpegStream_t = *mut c_void;
/// `nvjpegJpegDecoder_t`: one backend's decoder.
pub(crate) type nvjpegJpegDecoder_t = *mut c_void;
/// `nvjpegBufferPinned_t`.
pub(crate) type nvjpegBufferPinned_t = *mut c_void;
/// `nvjpegBufferDevice_t`.
pub(crate) type nvjpegBufferDevice_t = *mut c_void;
/// `nvjpegDecodeParams_t`.
pub(crate) type nvjpegDecodeParams_t = *mut c_void;
/// `nvjpegBackend_t`.
pub(crate) type nvjpegBackend_t = c_int;
/// `nvjpegOutputFormat_t`.
pub(crate) type nvjpegOutputFormat_t = c_int;
/// `nvjpegScaleFactor_t`: `NVJPEG_SCALE_NONE` to `NVJPEG_SCALE_1_BY_8`, 0
/// to 3, the decode at 1, 1/2, 1/4 and 1/8. The header says it works with
/// the hardware backend alone.
pub(crate) type nvjpegScaleFactor_t = c_int;
/// `libraryPropertyType` (`library_types.h`).
pub(crate) type libraryPropertyType = c_int;
/// `cudaStream_t`: the runtime's name for a driver stream, the same handle.
pub(crate) type cudaStream_t = CUstream;

pub(crate) const NVJPEG_STATUS_SUCCESS: nvjpegStatus_t = 0;
pub(crate) const NVJPEG_STATUS_BAD_JPEG: nvjpegStatus_t = 3;
pub(crate) const NVJPEG_STATUS_JPEG_NOT_SUPPORTED: nvjpegStatus_t = 4;
pub(crate) const NVJPEG_STATUS_INCOMPLETE_BITSTREAM: nvjpegStatus_t = 10;

/// `NVJPEG_BACKEND_HARDWARE`: the GPU's JPEG decode engine, for baseline
/// streams in one scan.
pub(crate) const NVJPEG_BACKEND_HARDWARE: nvjpegBackend_t = 3;

/// `NVJPEG_OUTPUT_YUV`: the components' planes, each at its own sampling.
pub(crate) const NVJPEG_OUTPUT_YUV: nvjpegOutputFormat_t = 1;
/// `NVJPEG_OUTPUT_BGRI`: interleaved B, G, R in the first channel.
pub(crate) const NVJPEG_OUTPUT_BGRI: nvjpegOutputFormat_t = 6;

/// `NVJPEG_FLAGS_UPSAMPLING_WITH_INTERPOLATION`: chroma interpolated, not
/// repeated, on its way to RGB.
pub(crate) const NVJPEG_FLAGS_UPSAMPLING_WITH_INTERPOLATION: c_uint = 1 << 5;

pub(crate) const MAJOR_VERSION: libraryPropertyType = 0;
pub(crate) const MINOR_VERSION: libraryPropertyType = 1;
pub(crate) const PATCH_LEVEL: libraryPropertyType = 2;

/// `NVJPEG_MAX_COMPONENT`.
pub(crate) const NVJPEG_MAX_COMPONENT: usize = 4;

/// `nvjpegImage_t`: where each output plane goes, in device memory.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct nvjpegImage_t {
    pub(crate) channel: [*mut u8; NVJPEG_MAX_COMPONENT],
    pub(crate) pitch: [usize; NVJPEG_MAX_COMPONENT],
}

// SAFETY: repr(C), of raw pointers and integers (`Zeroed`'s contract).
unsafe impl crate::dl::Zeroed for nvjpegImage_t {}

functions! {
    /// libnvjpeg's functions the decoder calls. The allocator arguments of
    /// `nvjpegCreateEx` and the buffers' creators are pointers to structs
    /// of NVIDIA's, always null here: the library's own allocators.
    pub(crate) struct NvjpegFunctions {
        nvjpegGetProperty: fn(libraryPropertyType, *mut c_int) -> nvjpegStatus_t
            = c"nvjpegGetProperty";
        nvjpegCreateEx: fn(
            nvjpegBackend_t,
            *mut c_void,
            *mut c_void,
            c_uint,
            *mut nvjpegHandle_t,
        ) -> nvjpegStatus_t = c"nvjpegCreateEx";
        nvjpegDestroy: fn(nvjpegHandle_t) -> nvjpegStatus_t = c"nvjpegDestroy";
        nvjpegGetHardwareDecoderInfo: fn(nvjpegHandle_t, *mut c_uint, *mut c_uint) -> nvjpegStatus_t
            = c"nvjpegGetHardwareDecoderInfo";
        nvjpegDecoderCreate: fn(nvjpegHandle_t, nvjpegBackend_t, *mut nvjpegJpegDecoder_t)
            -> nvjpegStatus_t = c"nvjpegDecoderCreate";
        nvjpegDecoderDestroy: fn(nvjpegJpegDecoder_t) -> nvjpegStatus_t = c"nvjpegDecoderDestroy";
        nvjpegDecoderStateCreate: fn(nvjpegHandle_t, nvjpegJpegDecoder_t, *mut nvjpegJpegState_t)
            -> nvjpegStatus_t = c"nvjpegDecoderStateCreate";
        nvjpegJpegStateDestroy: fn(nvjpegJpegState_t) -> nvjpegStatus_t
            = c"nvjpegJpegStateDestroy";
        nvjpegJpegStreamCreate: fn(nvjpegHandle_t, *mut nvjpegJpegStream_t) -> nvjpegStatus_t
            = c"nvjpegJpegStreamCreate";
        nvjpegJpegStreamDestroy: fn(nvjpegJpegStream_t) -> nvjpegStatus_t
            = c"nvjpegJpegStreamDestroy";
        nvjpegJpegStreamParse: fn(
            nvjpegHandle_t,
            *const u8,
            usize,
            c_int,
            c_int,
            nvjpegJpegStream_t,
        ) -> nvjpegStatus_t = c"nvjpegJpegStreamParse";
        nvjpegBufferPinnedCreate: fn(nvjpegHandle_t, *mut c_void, *mut nvjpegBufferPinned_t)
            -> nvjpegStatus_t = c"nvjpegBufferPinnedCreate";
        nvjpegBufferPinnedDestroy: fn(nvjpegBufferPinned_t) -> nvjpegStatus_t
            = c"nvjpegBufferPinnedDestroy";
        nvjpegBufferDeviceCreate: fn(nvjpegHandle_t, *mut c_void, *mut nvjpegBufferDevice_t)
            -> nvjpegStatus_t = c"nvjpegBufferDeviceCreate";
        nvjpegBufferDeviceDestroy: fn(nvjpegBufferDevice_t) -> nvjpegStatus_t
            = c"nvjpegBufferDeviceDestroy";
        nvjpegStateAttachPinnedBuffer: fn(nvjpegJpegState_t, nvjpegBufferPinned_t)
            -> nvjpegStatus_t = c"nvjpegStateAttachPinnedBuffer";
        nvjpegStateAttachDeviceBuffer: fn(nvjpegJpegState_t, nvjpegBufferDevice_t)
            -> nvjpegStatus_t = c"nvjpegStateAttachDeviceBuffer";
        nvjpegDecodeParamsCreate: fn(nvjpegHandle_t, *mut nvjpegDecodeParams_t) -> nvjpegStatus_t
            = c"nvjpegDecodeParamsCreate";
        nvjpegDecodeParamsDestroy: fn(nvjpegDecodeParams_t) -> nvjpegStatus_t
            = c"nvjpegDecodeParamsDestroy";
        nvjpegDecodeParamsSetOutputFormat: fn(nvjpegDecodeParams_t, nvjpegOutputFormat_t)
            -> nvjpegStatus_t = c"nvjpegDecodeParamsSetOutputFormat";
        nvjpegDecodeParamsSetROI: fn(nvjpegDecodeParams_t, c_int, c_int, c_int, c_int)
            -> nvjpegStatus_t = c"nvjpegDecodeParamsSetROI";
        nvjpegDecodeParamsSetScaleFactor: fn(nvjpegDecodeParams_t, nvjpegScaleFactor_t)
            -> nvjpegStatus_t = c"nvjpegDecodeParamsSetScaleFactor";
        nvjpegDecoderJpegSupported: fn(
            nvjpegJpegDecoder_t,
            nvjpegJpegStream_t,
            nvjpegDecodeParams_t,
            *mut c_int,
        ) -> nvjpegStatus_t = c"nvjpegDecoderJpegSupported";
        nvjpegDecodeJpegHost: fn(
            nvjpegHandle_t,
            nvjpegJpegDecoder_t,
            nvjpegJpegState_t,
            nvjpegDecodeParams_t,
            nvjpegJpegStream_t,
        ) -> nvjpegStatus_t = c"nvjpegDecodeJpegHost";
        nvjpegDecodeJpegTransferToDevice: fn(
            nvjpegHandle_t,
            nvjpegJpegDecoder_t,
            nvjpegJpegState_t,
            nvjpegJpegStream_t,
            cudaStream_t,
        ) -> nvjpegStatus_t = c"nvjpegDecodeJpegTransferToDevice";
        nvjpegDecodeJpegDevice: fn(
            nvjpegHandle_t,
            nvjpegJpegDecoder_t,
            nvjpegJpegState_t,
            *mut nvjpegImage_t,
            cudaStream_t,
        ) -> nvjpegStatus_t = c"nvjpegDecodeJpegDevice";
    }
}

/// The names libnvjpeg is looked for by, in order: CUDA 13's toolkit's,
/// then CUDA 12's. The functions above are in both; 13.2.3 is the one
/// measured.
const LIBRARY_NAMES: [&std::ffi::CStr; 2] = [c"libnvjpeg.so.13", c"libnvjpeg.so.12"];

/// libnvjpeg, loaded once per process.
pub(crate) struct Nvjpeg {
    pub(crate) f: NvjpegFunctions,
    _library: Library,
}

static NVJPEG: OnceLock<Result<Nvjpeg, LoadError>> = OnceLock::new();

impl Nvjpeg {
    /// libnvjpeg, loading it on the first call: the first of
    /// [`LIBRARY_NAMES`] the loader finds, or the error of the first name
    /// where it finds none.
    pub(crate) fn get() -> Result<&'static Self, LoadError> {
        NVJPEG
            .get_or_init(|| {
                let mut first_error = None;
                for name in LIBRARY_NAMES {
                    match Library::open(name) {
                        Ok(library) => {
                            return Ok(Self {
                                f: NvjpegFunctions::load(&library)?,
                                _library: library,
                            });
                        }
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                }
                Err(first_error.unwrap_or(LoadError {
                    library: "libnvjpeg",
                    symbol: None,
                    reason: "no name to look for".into(),
                }))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The library's version, as `major.minor.patch`.
    pub(crate) fn version(&self) -> String {
        let property = |kind| {
            let mut value: c_int = 0;
            // SAFETY: the pointer is to a live c_int; the call reads no
            // state of a handle.
            let status = unsafe { (self.f.nvjpegGetProperty)(kind, &raw mut value) };
            if status == NVJPEG_STATUS_SUCCESS {
                value
            } else {
                -1
            }
        };
        format!(
            "{}.{}.{}",
            property(MAJOR_VERSION),
            property(MINOR_VERSION),
            property(PATCH_LEVEL)
        )
    }
}

/// The name `nvjpeg.h` gives a status, for messages.
pub(crate) fn status_name(status: nvjpegStatus_t) -> &'static str {
    match status {
        0 => "NVJPEG_STATUS_SUCCESS",
        1 => "NVJPEG_STATUS_NOT_INITIALIZED",
        2 => "NVJPEG_STATUS_INVALID_PARAMETER",
        3 => "NVJPEG_STATUS_BAD_JPEG",
        4 => "NVJPEG_STATUS_JPEG_NOT_SUPPORTED",
        5 => "NVJPEG_STATUS_ALLOCATOR_FAILURE",
        6 => "NVJPEG_STATUS_EXECUTION_FAILED",
        7 => "NVJPEG_STATUS_ARCH_MISMATCH",
        8 => "NVJPEG_STATUS_INTERNAL_ERROR",
        9 => "NVJPEG_STATUS_IMPLEMENTATION_NOT_SUPPORTED",
        10 => "NVJPEG_STATUS_INCOMPLETE_BITSTREAM",
        _ => "an unknown status",
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<nvjpegImage_t>() == 64);
