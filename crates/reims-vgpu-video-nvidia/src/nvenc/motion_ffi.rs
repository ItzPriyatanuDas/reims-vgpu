//! NVENC's motion-estimation-only mode (`nvEncodeAPI.h`, "Structs needed
//! for ME only mode", `NvEncCreateMVBuffer`, `NvEncRunMotionEstimationOnly`),
//! at API 13.1, beside the encoder's declarations in [`super::ffi`]. Field
//! names are the header's, and `crate::layout`'s test checks each size,
//! offset and constant against ffnvcodec's `nvEncodeAPI.h` (13.1.15.0).

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    reason = "NVIDIA's names, kept for grepping the headers and for the layout check"
)]

use std::ffi::c_void;

use super::ffi::{
    NV_ENC_BUFFER_FORMAT, NV_ENC_CAPS, NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE,
    NVENCAPI_VERSION, Zeroed, bits::Bits,
};

/// `NVENCAPI_STRUCT_VERSION(ver)`, as [`super::ffi`] has it.
const fn struct_version(version: u32) -> u32 {
    NVENCAPI_VERSION | (version << 16) | (0x7 << 28)
}

/// `NV_ENC_CREATE_MV_BUFFER_VER`.
pub(crate) const NV_ENC_CREATE_MV_BUFFER_VER: u32 = struct_version(2);
/// `NV_ENC_MEONLY_PARAMS_VER`.
pub(crate) const NV_ENC_MEONLY_PARAMS_VER: u32 = struct_version(4);

/// `NV_ENC_CAPS_SUPPORT_MEONLY_MODE`: 1 where the codec's encoder runs
/// motion estimation alone.
pub(crate) const NV_ENC_CAPS_SUPPORT_MEONLY_MODE: NV_ENC_CAPS = 36;

/// `NV_ENC_INITIALIZE_PARAMS::enableMEOnlyMode`, in its `flags` word.
pub(crate) const enableMEOnlyMode: Bits = Bits { shift: 3, width: 1 };

/// `NV_ENC_MVECTOR`: one vector, each component in quarter pixels.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NV_ENC_MVECTOR {
    pub(crate) mvx: i16,
    pub(crate) mvy: i16,
}

/// `NV_ENC_H264_MV_DATA`: one macroblock's vectors, one for each of its
/// 8x8 blocks, its type (0 I, 1 P, 2 IPCM, 3 B) and its partition (0
/// 16x16, 1 8x8, 2 16x8, 3 8x16).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NV_ENC_H264_MV_DATA {
    pub(crate) mv: [NV_ENC_MVECTOR; 4],
    pub(crate) mbType: u8,
    pub(crate) partitionType: u8,
    pub(crate) reserved: u16,
    pub(crate) mbCost: u32,
}

/// `NV_ENC_CREATE_MV_BUFFER`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct NV_ENC_CREATE_MV_BUFFER {
    pub(crate) version: u32,
    pub(crate) reserved: u32,
    pub(crate) mvBuffer: *mut c_void,
    pub(crate) reserved1: [u32; 254],
    pub(crate) reserved2: [*mut c_void; 63],
}

/// `NV_ENC_MEONLY_PARAMS`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct NV_ENC_MEONLY_PARAMS {
    pub(crate) version: u32,
    pub(crate) inputWidth: u32,
    pub(crate) inputHeight: u32,
    pub(crate) reserved: u32,
    pub(crate) inputBuffer: *mut c_void,
    pub(crate) referenceFrame: *mut c_void,
    pub(crate) mvBuffer: *mut c_void,
    pub(crate) reserved2: u32,
    pub(crate) bufferFmt: NV_ENC_BUFFER_FORMAT,
    pub(crate) completionEvent: *mut c_void,
    pub(crate) viewID: u32,
    pub(crate) meHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
    pub(crate) meExternalHints: *mut c_void,
    pub(crate) reserved1: [u32; 241],
    pub(crate) reserved3: [*mut c_void; 59],
}

// SAFETY: each is repr(C) and holds only integers, raw pointers, and
// arrays and structs of those (`Zeroed`'s contract).
unsafe impl Zeroed for NV_ENC_CREATE_MV_BUFFER {}
// SAFETY: as above.
unsafe impl Zeroed for NV_ENC_MEONLY_PARAMS {}

// The sizes on x86-64; `crate::layout`'s test checks them, and every
// offset, against the header.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<NV_ENC_MVECTOR>() == 4);
    assert!(size_of::<NV_ENC_H264_MV_DATA>() == 24);
    assert!(size_of::<NV_ENC_CREATE_MV_BUFFER>() == 1536);
    assert!(size_of::<NV_ENC_MEONLY_PARAMS>() == 1544);
};
