//! NVDEC's declarations (libnvcuvid's decoder half, `cuviddec.h`), the
//! subset this crate uses: capabilities, the decoder, picture parameters
//! for H.264, HEVC, VP9 and AV1, mapping and status. NVIDIA's parser
//! (`nvcuvid.h`) is not used: our own parsers fill the picture parameters.
//!
//! Field names are the header's, so that `crate::layout`'s test can check
//! each size, offset and bit-field against ffnvcodec's `dynlink_cuviddec.h`
//! (NVDECAPI 13.1). A C bit-field word is one Rust integer, named for what
//! it holds, with its fields' positions in the [`bits`] submodules. Those
//! words' names, and the names of the header's anonymous structs and
//! unions, are ours, each marked so.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::upper_case_acronyms,
    reason = "NVIDIA's names, kept for grepping the headers and for the layout check"
)]

use std::ffi::{c_int, c_short, c_uint, c_ulong, c_ulonglong, c_ushort, c_void};

use crate::cuda::ffi::{CUresult, CUstream};
pub(crate) use crate::dl::Zeroed;
use crate::dl::functions;

/// `CUvideodecoder`.
pub(crate) type CUvideodecoder = *mut c_void;
/// `CUvideoctxlock`.
pub(crate) type CUvideoctxlock = *mut c_void;
/// `CUvideotimestamp`.
pub(crate) type CUvideotimestamp = i64;
/// `cudaVideoCodec`.
pub(crate) type cudaVideoCodec = c_uint;
/// `cudaVideoChromaFormat`.
pub(crate) type cudaVideoChromaFormat = c_uint;
/// `cudaVideoSurfaceFormat`.
pub(crate) type cudaVideoSurfaceFormat = c_uint;
/// `cudaVideoDeinterlaceMode`.
pub(crate) type cudaVideoDeinterlaceMode = c_uint;
/// `cuvidDecodeStatus`.
pub(crate) type cuvidDecodeStatus = c_uint;

pub(crate) const cudaVideoCodec_H264: cudaVideoCodec = 4;
pub(crate) const cudaVideoCodec_JPEG: cudaVideoCodec = 5;
pub(crate) const cudaVideoCodec_HEVC: cudaVideoCodec = 8;
pub(crate) const cudaVideoCodec_VP8: cudaVideoCodec = 9;
pub(crate) const cudaVideoCodec_VP9: cudaVideoCodec = 10;
pub(crate) const cudaVideoCodec_AV1: cudaVideoCodec = 11;
pub(crate) const cudaVideoCodec_MPEG2: cudaVideoCodec = 1;

pub(crate) const cudaVideoChromaFormat_Monochrome: cudaVideoChromaFormat = 0;
pub(crate) const cudaVideoChromaFormat_420: cudaVideoChromaFormat = 1;
pub(crate) const cudaVideoChromaFormat_422: cudaVideoChromaFormat = 2;
pub(crate) const cudaVideoChromaFormat_444: cudaVideoChromaFormat = 3;

pub(crate) const cudaVideoSurfaceFormat_NV12: cudaVideoSurfaceFormat = 0;
pub(crate) const cudaVideoSurfaceFormat_P016: cudaVideoSurfaceFormat = 1;
pub(crate) const cudaVideoSurfaceFormat_YUV444: cudaVideoSurfaceFormat = 2;
pub(crate) const cudaVideoSurfaceFormat_YUV444_16Bit: cudaVideoSurfaceFormat = 3;
pub(crate) const cudaVideoSurfaceFormat_NV16: cudaVideoSurfaceFormat = 4;
pub(crate) const cudaVideoSurfaceFormat_P216: cudaVideoSurfaceFormat = 5;

pub(crate) const cudaVideoDeinterlaceMode_Weave: cudaVideoDeinterlaceMode = 0;

/// `cudaVideoCreate_PreferCUVID`: decode on the NVDEC engine, with no CUDA
/// post-processing or context lock.
pub(crate) const cudaVideoCreate_PreferCUVID: c_ulong = 0x04;

pub(crate) const cuvidDecodeStatus_InProgress: cuvidDecodeStatus = 1;
pub(crate) const cuvidDecodeStatus_Error: cuvidDecodeStatus = 8;
pub(crate) const cuvidDecodeStatus_Error_Concealed: cuvidDecodeStatus = 9;

/// `CUVIDDECODECAPS`: what NVDEC decodes of one codec, chroma format and
/// bit depth. The first three fields are in, the rest out.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDDECODECAPS {
    pub(crate) eCodecType: cudaVideoCodec,
    pub(crate) eChromaFormat: cudaVideoChromaFormat,
    pub(crate) nBitDepthMinus8: c_uint,
    pub(crate) reserved1: [c_uint; 3],
    pub(crate) bIsSupported: u8,
    pub(crate) nNumNVDECs: u8,
    pub(crate) nOutputFormatMask: c_ushort,
    pub(crate) nMaxWidth: c_uint,
    pub(crate) nMaxHeight: c_uint,
    pub(crate) nMaxMBCount: c_uint,
    pub(crate) nMinWidth: c_ushort,
    pub(crate) nMinHeight: c_ushort,
    pub(crate) bIsHistogramSupported: u8,
    pub(crate) nCounterBitDepth: u8,
    pub(crate) nMaxHistogramBins: c_ushort,
    pub(crate) bIsDecodeStatsSupported: u8,
    pub(crate) reserved4: [u8; 3],
    pub(crate) reserved3: [c_uint; 9],
}

/// A rectangle of `CUVIDDECODECREATEINFO`. Name is ours: the header's
/// `display_area` and `target_rect` are anonymous structs.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CuvidRect {
    pub(crate) left: c_short,
    pub(crate) top: c_short,
    pub(crate) right: c_short,
    pub(crate) bottom: c_short,
}

/// `CUVIDDECODECREATEINFO`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDDECODECREATEINFO {
    pub(crate) ulWidth: c_ulong,
    pub(crate) ulHeight: c_ulong,
    pub(crate) ulNumDecodeSurfaces: c_ulong,
    pub(crate) CodecType: cudaVideoCodec,
    pub(crate) ChromaFormat: cudaVideoChromaFormat,
    pub(crate) ulCreationFlags: c_ulong,
    pub(crate) bitDepthMinus8: c_ulong,
    pub(crate) ulIntraDecodeOnly: c_ulong,
    pub(crate) ulMaxWidth: c_ulong,
    pub(crate) ulMaxHeight: c_ulong,
    pub(crate) Reserved1: c_ulong,
    pub(crate) display_area: CuvidRect,
    pub(crate) OutputFormat: cudaVideoSurfaceFormat,
    pub(crate) DeinterlaceMode: cudaVideoDeinterlaceMode,
    pub(crate) ulTargetWidth: c_ulong,
    pub(crate) ulTargetHeight: c_ulong,
    pub(crate) ulNumOutputSurfaces: c_ulong,
    pub(crate) vidLock: CUvideoctxlock,
    pub(crate) target_rect: CuvidRect,
    pub(crate) enableHistogram: c_ulong,
    pub(crate) enableDecodeFeatures: c_ulong,
    pub(crate) Reserved2: [c_ulong; 3],
}

/// `CUVIDH264DPBENTRY`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CUVIDH264DPBENTRY {
    pub(crate) PicIdx: c_int,
    pub(crate) FrameIdx: c_int,
    pub(crate) is_long_term: c_int,
    pub(crate) not_existing: c_int,
    pub(crate) used_for_reference: c_int,
    pub(crate) FieldOrderCnt: [c_int; 2],
}

/// `CUVIDH264PICPARAMS`. Its last member, an anonymous union of the MVC
/// and SVC extensions, is held as its bytes: neither is used.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDH264PICPARAMS {
    pub(crate) log2_max_frame_num_minus4: c_int,
    pub(crate) pic_order_cnt_type: c_int,
    pub(crate) log2_max_pic_order_cnt_lsb_minus4: c_int,
    pub(crate) delta_pic_order_always_zero_flag: c_int,
    pub(crate) frame_mbs_only_flag: c_int,
    pub(crate) direct_8x8_inference_flag: c_int,
    pub(crate) num_ref_frames: c_int,
    pub(crate) residual_colour_transform_flag: u8,
    pub(crate) bit_depth_luma_minus8: u8,
    pub(crate) bit_depth_chroma_minus8: u8,
    pub(crate) qpprime_y_zero_transform_bypass_flag: u8,
    pub(crate) entropy_coding_mode_flag: c_int,
    pub(crate) pic_order_present_flag: c_int,
    pub(crate) num_ref_idx_l0_active_minus1: c_int,
    pub(crate) num_ref_idx_l1_active_minus1: c_int,
    pub(crate) weighted_pred_flag: c_int,
    pub(crate) weighted_bipred_idc: c_int,
    pub(crate) pic_init_qp_minus26: c_int,
    pub(crate) deblocking_filter_control_present_flag: c_int,
    pub(crate) redundant_pic_cnt_present_flag: c_int,
    pub(crate) transform_8x8_mode_flag: c_int,
    pub(crate) MbaffFrameFlag: c_int,
    pub(crate) constrained_intra_pred_flag: c_int,
    pub(crate) chroma_qp_index_offset: c_int,
    pub(crate) second_chroma_qp_index_offset: c_int,
    pub(crate) ref_pic_flag: c_int,
    pub(crate) frame_num: c_int,
    pub(crate) CurrFieldOrderCnt: [c_int; 2],
    pub(crate) dpb: [CUVIDH264DPBENTRY; 16],
    pub(crate) WeightScale4x4: [[u8; 16]; 6],
    pub(crate) WeightScale8x8: [[u8; 64]; 2],
    pub(crate) fmo_aso_enable: u8,
    pub(crate) num_slice_groups_minus1: u8,
    pub(crate) slice_group_map_type: u8,
    pub(crate) pic_init_qs_minus26: i8,
    pub(crate) slice_group_change_rate_minus1: c_uint,
    /// A union of `slice_group_map_addr` and `pMb2SliceGroupMap`.
    pub(crate) fmo: c_ulonglong,
    /// `mb_adaptive_frame_field_flag : 2` and `Reserved1 : 30`: [`bits::h264`].
    /// Name is ours.
    pub(crate) mbaff_word: c_uint,
    pub(crate) Reserved: [c_uint; 11],
    /// The anonymous union of `mvcext` and `svcext`. Name is ours: its
    /// first member's.
    pub(crate) mvcext: [u64; 18],
}

/// `CUVIDHEVCPICPARAMS`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDHEVCPICPARAMS {
    pub(crate) pic_width_in_luma_samples: c_int,
    pub(crate) pic_height_in_luma_samples: c_int,
    pub(crate) log2_min_luma_coding_block_size_minus3: u8,
    pub(crate) log2_diff_max_min_luma_coding_block_size: u8,
    pub(crate) log2_min_transform_block_size_minus2: u8,
    pub(crate) log2_diff_max_min_transform_block_size: u8,
    pub(crate) pcm_enabled_flag: u8,
    pub(crate) log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub(crate) log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub(crate) pcm_sample_bit_depth_luma_minus1: u8,
    pub(crate) pcm_sample_bit_depth_chroma_minus1: u8,
    pub(crate) pcm_loop_filter_disabled_flag: u8,
    pub(crate) strong_intra_smoothing_enabled_flag: u8,
    pub(crate) max_transform_hierarchy_depth_intra: u8,
    pub(crate) max_transform_hierarchy_depth_inter: u8,
    pub(crate) amp_enabled_flag: u8,
    pub(crate) separate_colour_plane_flag: u8,
    pub(crate) log2_max_pic_order_cnt_lsb_minus4: u8,
    pub(crate) num_short_term_ref_pic_sets: u8,
    pub(crate) long_term_ref_pics_present_flag: u8,
    pub(crate) num_long_term_ref_pics_sps: u8,
    pub(crate) sps_temporal_mvp_enabled_flag: u8,
    pub(crate) sample_adaptive_offset_enabled_flag: u8,
    pub(crate) scaling_list_enable_flag: u8,
    pub(crate) IrapPicFlag: u8,
    pub(crate) IdrPicFlag: u8,
    pub(crate) bit_depth_luma_minus8: u8,
    pub(crate) bit_depth_chroma_minus8: u8,
    pub(crate) log2_max_transform_skip_block_size_minus2: u8,
    pub(crate) log2_sao_offset_scale_luma: u8,
    pub(crate) log2_sao_offset_scale_chroma: u8,
    pub(crate) high_precision_offsets_enabled_flag: u8,
    pub(crate) mv_hevc_enable: u8,
    pub(crate) nuh_layer_id: u8,
    pub(crate) default_ref_layers_active_flag: u8,
    pub(crate) NumDirectRefLayers: u8,
    pub(crate) max_one_active_ref_layer_flag: u8,
    pub(crate) poc_lsb_not_present_flag: u8,
    pub(crate) NumActiveRefLayerPics0: u8,
    pub(crate) NumActiveRefLayerPics1: u8,
    pub(crate) reserved1: [u8; 2],
    pub(crate) dependent_slice_segments_enabled_flag: u8,
    pub(crate) slice_segment_header_extension_present_flag: u8,
    pub(crate) sign_data_hiding_enabled_flag: u8,
    pub(crate) cu_qp_delta_enabled_flag: u8,
    pub(crate) diff_cu_qp_delta_depth: u8,
    pub(crate) init_qp_minus26: i8,
    pub(crate) pps_cb_qp_offset: i8,
    pub(crate) pps_cr_qp_offset: i8,
    pub(crate) constrained_intra_pred_flag: u8,
    pub(crate) weighted_pred_flag: u8,
    pub(crate) weighted_bipred_flag: u8,
    pub(crate) transform_skip_enabled_flag: u8,
    pub(crate) transquant_bypass_enabled_flag: u8,
    pub(crate) entropy_coding_sync_enabled_flag: u8,
    pub(crate) log2_parallel_merge_level_minus2: u8,
    pub(crate) num_extra_slice_header_bits: u8,
    pub(crate) loop_filter_across_tiles_enabled_flag: u8,
    pub(crate) loop_filter_across_slices_enabled_flag: u8,
    pub(crate) output_flag_present_flag: u8,
    pub(crate) num_ref_idx_l0_default_active_minus1: u8,
    pub(crate) num_ref_idx_l1_default_active_minus1: u8,
    pub(crate) lists_modification_present_flag: u8,
    pub(crate) cabac_init_present_flag: u8,
    pub(crate) pps_slice_chroma_qp_offsets_present_flag: u8,
    pub(crate) deblocking_filter_override_enabled_flag: u8,
    pub(crate) pps_deblocking_filter_disabled_flag: u8,
    pub(crate) pps_beta_offset_div2: i8,
    pub(crate) pps_tc_offset_div2: i8,
    pub(crate) tiles_enabled_flag: u8,
    pub(crate) uniform_spacing_flag: u8,
    pub(crate) num_tile_columns_minus1: u8,
    pub(crate) num_tile_rows_minus1: u8,
    pub(crate) column_width_minus1: [c_ushort; 21],
    pub(crate) row_height_minus1: [c_ushort; 21],
    pub(crate) sps_range_extension_flag: u8,
    pub(crate) transform_skip_rotation_enabled_flag: u8,
    pub(crate) transform_skip_context_enabled_flag: u8,
    pub(crate) implicit_rdpcm_enabled_flag: u8,
    pub(crate) explicit_rdpcm_enabled_flag: u8,
    pub(crate) extended_precision_processing_flag: u8,
    pub(crate) intra_smoothing_disabled_flag: u8,
    pub(crate) persistent_rice_adaptation_enabled_flag: u8,
    pub(crate) cabac_bypass_alignment_enabled_flag: u8,
    pub(crate) pps_range_extension_flag: u8,
    pub(crate) cross_component_prediction_enabled_flag: u8,
    pub(crate) chroma_qp_offset_list_enabled_flag: u8,
    pub(crate) diff_cu_chroma_qp_offset_depth: u8,
    pub(crate) chroma_qp_offset_list_len_minus1: u8,
    pub(crate) cb_qp_offset_list: [i8; 6],
    pub(crate) cr_qp_offset_list: [i8; 6],
    pub(crate) reserved2: [u8; 2],
    pub(crate) reserved3: [c_uint; 8],
    pub(crate) NumBitsForShortTermRPSInSlice: c_int,
    pub(crate) NumDeltaPocsOfRefRpsIdx: c_int,
    pub(crate) NumPocTotalCurr: c_int,
    pub(crate) NumPocStCurrBefore: c_int,
    pub(crate) NumPocStCurrAfter: c_int,
    pub(crate) NumPocLtCurr: c_int,
    pub(crate) CurrPicOrderCntVal: c_int,
    pub(crate) RefPicIdx: [c_int; 16],
    pub(crate) PicOrderCntVal: [c_int; 16],
    pub(crate) IsLongTerm: [u8; 16],
    pub(crate) RefPicSetStCurrBefore: [u8; 8],
    pub(crate) RefPicSetStCurrAfter: [u8; 8],
    pub(crate) RefPicSetLtCurr: [u8; 8],
    pub(crate) RefPicSetInterLayer0: [u8; 8],
    pub(crate) RefPicSetInterLayer1: [u8; 8],
    pub(crate) reserved4: [c_uint; 12],
    pub(crate) ScalingList4x4: [[u8; 16]; 6],
    pub(crate) ScalingList8x8: [[u8; 64]; 6],
    pub(crate) ScalingList16x16: [[u8; 64]; 6],
    pub(crate) ScalingList32x32: [[u8; 64]; 2],
    pub(crate) ScalingListDCCoeff16x16: [u8; 6],
    pub(crate) ScalingListDCCoeff32x32: [u8; 2],
}

/// `CUVIDVP9PICPARAMS`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDVP9PICPARAMS {
    pub(crate) width: c_uint,
    pub(crate) height: c_uint,
    pub(crate) LastRefIdx: u8,
    pub(crate) GoldenRefIdx: u8,
    pub(crate) AltRefIdx: u8,
    pub(crate) colorSpace: u8,
    /// `profile` to `reserved2Bits`: [`bits::vp9_frame`]. Name is ours.
    pub(crate) frame_word: c_ushort,
    pub(crate) reserved16Bits: c_ushort,
    pub(crate) refFrameSignBias: [u8; 4],
    pub(crate) bitDepthMinus8Luma: u8,
    pub(crate) bitDepthMinus8Chroma: u8,
    pub(crate) loopFilterLevel: u8,
    pub(crate) loopFilterSharpness: u8,
    pub(crate) modeRefLfEnabled: u8,
    pub(crate) log2_tile_columns: u8,
    pub(crate) log2_tile_rows: u8,
    /// `segmentEnabled` to `reserved4Bits`: [`bits::vp9_segment`]. Name is
    /// ours.
    pub(crate) segment_word: u8,
    pub(crate) segmentFeatureEnable: [[u8; 4]; 8],
    pub(crate) segmentFeatureData: [[c_short; 4]; 8],
    pub(crate) mb_segment_tree_probs: [u8; 7],
    pub(crate) segment_pred_probs: [u8; 3],
    pub(crate) reservedSegment16Bits: [u8; 2],
    pub(crate) qpYAc: c_int,
    pub(crate) qpYDc: c_int,
    pub(crate) qpChDc: c_int,
    pub(crate) qpChAc: c_int,
    pub(crate) activeRefIdx: [c_uint; 3],
    pub(crate) resetFrameContext: c_uint,
    pub(crate) mcomp_filter_type: c_uint,
    pub(crate) mbRefLfDelta: [c_uint; 4],
    pub(crate) mbModeLfDelta: [c_uint; 2],
    pub(crate) frameTagSize: c_uint,
    pub(crate) offsetToDctParts: c_uint,
    pub(crate) reserved128Bits: [c_uint; 4],
}

/// One entry of `CUVIDAV1PICPARAMS::ref_frame`. Name is ours: an anonymous
/// struct in the header.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CuvidAv1RefFrame {
    pub(crate) width: c_uint,
    pub(crate) height: c_uint,
    pub(crate) index: u8,
    pub(crate) reserved24Bits: [u8; 3],
}

/// One entry of `CUVIDAV1PICPARAMS::global_motion`. Name is ours: an
/// anonymous struct in the header.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CuvidAv1GlobalMotion {
    /// `invalid : 1`, `wmtype : 2`, `reserved5Bits : 5`:
    /// [`bits::av1_global_motion`]. Name is ours.
    pub(crate) flags: u8,
    pub(crate) reserved24Bits: [i8; 3],
    pub(crate) wmmat: [c_int; 6],
}

/// `CUVIDAV1PICPARAMS`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDAV1PICPARAMS {
    pub(crate) width: c_uint,
    pub(crate) height: c_uint,
    pub(crate) frame_offset: c_uint,
    pub(crate) decodePicIdx: c_int,
    /// The sequence header's flags: [`bits::av1_sequence`]. Name is ours.
    pub(crate) sequence_word: c_uint,
    /// The frame header's flags: [`bits::av1_frame`]. Name is ours.
    pub(crate) frame_word: c_uint,
    /// `num_tile_cols : 8`, `num_tile_rows : 8`,
    /// `context_update_tile_id : 16`: [`bits::av1_tiling`]. Name is ours.
    pub(crate) tiling_word: c_uint,
    pub(crate) tile_widths: [c_ushort; 64],
    pub(crate) tile_heights: [c_ushort; 64],
    /// `cdef_damping_minus_3 : 2`, `cdef_bits : 2`: [`bits::av1_cdef`].
    /// Name is ours.
    pub(crate) cdef_word: u8,
    pub(crate) cdef_y_strength: [u8; 8],
    pub(crate) cdef_uv_strength: [u8; 8],
    /// `SkipModeFrame0 : 4`, `SkipModeFrame1 : 4`:
    /// [`bits::av1_skip_mode`]. Name is ours.
    pub(crate) skip_mode_word: u8,
    pub(crate) base_qindex: u8,
    pub(crate) qp_y_dc_delta_q: i8,
    pub(crate) qp_u_dc_delta_q: i8,
    pub(crate) qp_v_dc_delta_q: i8,
    pub(crate) qp_u_ac_delta_q: i8,
    pub(crate) qp_v_ac_delta_q: i8,
    pub(crate) qm_y: u8,
    pub(crate) qm_u: u8,
    pub(crate) qm_v: u8,
    /// The segmentation flags: [`bits::av1_segmentation`]. Name is ours.
    pub(crate) segmentation_word: u8,
    pub(crate) segmentation_feature_data: [[c_short; 8]; 8],
    pub(crate) segmentation_feature_mask: [u8; 8],
    pub(crate) loop_filter_level: [u8; 2],
    pub(crate) loop_filter_level_u: u8,
    pub(crate) loop_filter_level_v: u8,
    pub(crate) loop_filter_sharpness: u8,
    pub(crate) loop_filter_ref_deltas: [i8; 8],
    pub(crate) loop_filter_mode_deltas: [i8; 2],
    /// The loop filter's delta flags: [`bits::av1_loop_filter`]. Name is
    /// ours.
    pub(crate) loop_filter_word: u8,
    pub(crate) lr_unit_size: [u8; 3],
    pub(crate) lr_type: [u8; 3],
    pub(crate) primary_ref_frame: u8,
    pub(crate) ref_frame_map: [u8; 8],
    /// `temporal_layer_id : 4`, `spatial_layer_id : 4`:
    /// [`bits::av1_layer`]. Name is ours.
    pub(crate) layer_word: u8,
    pub(crate) reserved5_32bits: [u8; 4],
    pub(crate) ref_frame: [CuvidAv1RefFrame; 7],
    pub(crate) global_motion: [CuvidAv1GlobalMotion; 7],
    /// The film grain flags: [`bits::av1_film_grain`]. Name is ours.
    pub(crate) film_grain_word: c_ushort,
    pub(crate) num_y_points: u8,
    pub(crate) scaling_points_y: [[u8; 2]; 14],
    pub(crate) num_cb_points: u8,
    pub(crate) scaling_points_cb: [[u8; 2]; 10],
    pub(crate) num_cr_points: u8,
    pub(crate) scaling_points_cr: [[u8; 2]; 10],
    pub(crate) reserved7_8bits: u8,
    pub(crate) random_seed: c_ushort,
    pub(crate) ar_coeffs_y: [c_short; 24],
    pub(crate) ar_coeffs_cb: [c_short; 25],
    pub(crate) ar_coeffs_cr: [c_short; 25],
    pub(crate) cb_mult: u8,
    pub(crate) cb_luma_mult: u8,
    pub(crate) cb_offset: c_short,
    pub(crate) cr_mult: u8,
    pub(crate) cr_luma_mult: u8,
    pub(crate) cr_offset: c_short,
    pub(crate) reserved: [c_int; 7],
}

/// `CUVIDPICPARAMS::CodecSpecific`. Name is ours: an anonymous union in the
/// header.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) union CuvidCodecSpecific {
    pub(crate) h264: CUVIDH264PICPARAMS,
    pub(crate) hevc: CUVIDHEVCPICPARAMS,
    pub(crate) vp9: CUVIDVP9PICPARAMS,
    pub(crate) av1: CUVIDAV1PICPARAMS,
    pub(crate) CodecReserved: [c_uint; 1024],
}

/// `CUVIDPICPARAMS`: one picture to decode.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct CUVIDPICPARAMS {
    pub(crate) PicWidthInMbs: c_int,
    pub(crate) FrameHeightInMbs: c_int,
    pub(crate) CurrPicIdx: c_int,
    pub(crate) field_pic_flag: c_int,
    pub(crate) bottom_field_flag: c_int,
    pub(crate) second_field: c_int,
    pub(crate) nBitstreamDataLen: c_uint,
    pub(crate) pBitstreamData: *const u8,
    pub(crate) nNumSlices: c_uint,
    pub(crate) pSliceDataOffsets: *const c_uint,
    pub(crate) ref_pic_flag: c_int,
    pub(crate) intra_pic_flag: c_int,
    pub(crate) timestamp: CUvideotimestamp,
    pub(crate) Reserved: [c_uint; 28],
    pub(crate) CodecSpecific: CuvidCodecSpecific,
}

/// `CUVIDPROCPARAMS`: how to map a decoded picture.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDPROCPARAMS {
    pub(crate) progressive_frame: c_int,
    pub(crate) second_field: c_int,
    pub(crate) top_field_first: c_int,
    pub(crate) unpaired_field: c_int,
    pub(crate) reserved_flags: c_uint,
    pub(crate) reserved_zero: c_uint,
    pub(crate) raw_input_dptr: c_ulonglong,
    pub(crate) raw_input_pitch: c_uint,
    pub(crate) raw_input_format: c_uint,
    pub(crate) raw_output_dptr: c_ulonglong,
    pub(crate) raw_output_pitch: c_uint,
    pub(crate) Reserved1: c_uint,
    pub(crate) output_stream: CUstream,
    pub(crate) Reserved: [c_uint; 46],
    pub(crate) histogram_dptr: *mut c_ulonglong,
    pub(crate) pCuvidProcExt: *mut c_void,
}

/// `CUVIDGETDECODESTATUS`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDGETDECODESTATUS {
    pub(crate) decodeStatus: cuvidDecodeStatus,
    pub(crate) reserved: [c_uint; 31],
    pub(crate) pReserved: [*mut c_void; 8],
}

/// `CUVIDRECONFIGUREDECODERINFO`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CUVIDRECONFIGUREDECODERINFO {
    pub(crate) ulWidth: c_uint,
    pub(crate) ulHeight: c_uint,
    pub(crate) ulTargetWidth: c_uint,
    pub(crate) ulTargetHeight: c_uint,
    pub(crate) ulNumDecodeSurfaces: c_uint,
    pub(crate) reserved1: [c_uint; 12],
    pub(crate) display_area: CuvidRect,
    pub(crate) target_rect: CuvidRect,
    pub(crate) reserved2: [c_uint; 11],
}

/// The positions of the C bit-fields, in the Rust words that hold them:
/// GCC's x86-64 layout, each field from the least significant bit up in
/// declaration order (checked by `crate::layout`'s test).
pub(crate) mod bits {
    /// A bit-field: `width` bits from bit `shift` of its word.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Bits {
        pub(crate) shift: u32,
        pub(crate) width: u32,
    }

    impl Bits {
        /// The field's mask in its word.
        pub(crate) const fn mask(self) -> u32 {
            (u32::MAX >> (32 - self.width)) << self.shift
        }

        /// `word` with the field set to `value`, cut to the field's width.
        pub(crate) const fn put(self, word: u32, value: u32) -> u32 {
            (word & !self.mask()) | ((value << self.shift) & self.mask())
        }
    }

    const fn b(shift: u32, width: u32) -> Bits {
        Bits { shift, width }
    }

    /// `CUVIDH264PICPARAMS::mb_adaptive_frame_field_flag`.
    pub(crate) mod h264 {
        use super::{Bits, b};
        pub(crate) const mb_adaptive_frame_field_flag: Bits = b(0, 2);
    }

    /// `CUVIDVP9PICPARAMS`'s frame flags (an `unsigned short`).
    pub(crate) mod vp9_frame {
        use super::{Bits, b};
        pub(crate) const profile: Bits = b(0, 3);
        pub(crate) const frameContextIdx: Bits = b(3, 2);
        pub(crate) const frameType: Bits = b(5, 1);
        pub(crate) const showFrame: Bits = b(6, 1);
        pub(crate) const errorResilient: Bits = b(7, 1);
        pub(crate) const frameParallelDecoding: Bits = b(8, 1);
        pub(crate) const subSamplingX: Bits = b(9, 1);
        pub(crate) const subSamplingY: Bits = b(10, 1);
        pub(crate) const intraOnly: Bits = b(11, 1);
        pub(crate) const allow_high_precision_mv: Bits = b(12, 1);
        pub(crate) const refreshEntropyProbs: Bits = b(13, 1);
    }

    /// `CUVIDVP9PICPARAMS`'s segmentation flags (an `unsigned char`).
    pub(crate) mod vp9_segment {
        use super::{Bits, b};
        pub(crate) const segmentEnabled: Bits = b(0, 1);
        pub(crate) const segmentMapUpdate: Bits = b(1, 1);
        pub(crate) const segmentMapTemporalUpdate: Bits = b(2, 1);
        pub(crate) const segmentFeatureMode: Bits = b(3, 1);
    }

    /// `CUVIDAV1PICPARAMS`'s sequence header flags.
    pub(crate) mod av1_sequence {
        use super::{Bits, b};
        pub(crate) const profile: Bits = b(0, 3);
        pub(crate) const use_128x128_superblock: Bits = b(3, 1);
        pub(crate) const subsampling_x: Bits = b(4, 1);
        pub(crate) const subsampling_y: Bits = b(5, 1);
        pub(crate) const mono_chrome: Bits = b(6, 1);
        pub(crate) const bit_depth_minus8: Bits = b(7, 4);
        pub(crate) const enable_filter_intra: Bits = b(11, 1);
        pub(crate) const enable_intra_edge_filter: Bits = b(12, 1);
        pub(crate) const enable_interintra_compound: Bits = b(13, 1);
        pub(crate) const enable_masked_compound: Bits = b(14, 1);
        pub(crate) const enable_dual_filter: Bits = b(15, 1);
        pub(crate) const enable_order_hint: Bits = b(16, 1);
        pub(crate) const order_hint_bits_minus1: Bits = b(17, 3);
        pub(crate) const enable_jnt_comp: Bits = b(20, 1);
        pub(crate) const enable_superres: Bits = b(21, 1);
        pub(crate) const enable_cdef: Bits = b(22, 1);
        pub(crate) const enable_restoration: Bits = b(23, 1);
        pub(crate) const enable_fgs: Bits = b(24, 1);
    }

    /// `CUVIDAV1PICPARAMS`'s frame header flags.
    pub(crate) mod av1_frame {
        use super::{Bits, b};
        pub(crate) const frame_type: Bits = b(0, 2);
        pub(crate) const show_frame: Bits = b(2, 1);
        pub(crate) const disable_cdf_update: Bits = b(3, 1);
        pub(crate) const allow_screen_content_tools: Bits = b(4, 1);
        pub(crate) const force_integer_mv: Bits = b(5, 1);
        pub(crate) const coded_denom: Bits = b(6, 3);
        pub(crate) const allow_intrabc: Bits = b(9, 1);
        pub(crate) const allow_high_precision_mv: Bits = b(10, 1);
        pub(crate) const interp_filter: Bits = b(11, 3);
        pub(crate) const switchable_motion_mode: Bits = b(14, 1);
        pub(crate) const use_ref_frame_mvs: Bits = b(15, 1);
        pub(crate) const disable_frame_end_update_cdf: Bits = b(16, 1);
        pub(crate) const delta_q_present: Bits = b(17, 1);
        pub(crate) const delta_q_res: Bits = b(18, 2);
        pub(crate) const using_qmatrix: Bits = b(20, 1);
        pub(crate) const coded_lossless: Bits = b(21, 1);
        pub(crate) const use_superres: Bits = b(22, 1);
        pub(crate) const tx_mode: Bits = b(23, 2);
        pub(crate) const reference_mode: Bits = b(25, 1);
        pub(crate) const allow_warped_motion: Bits = b(26, 1);
        pub(crate) const reduced_tx_set: Bits = b(27, 1);
        pub(crate) const skip_mode: Bits = b(28, 1);
    }

    /// `CUVIDAV1PICPARAMS`'s tiling word.
    pub(crate) mod av1_tiling {
        use super::{Bits, b};
        pub(crate) const num_tile_cols: Bits = b(0, 8);
        pub(crate) const num_tile_rows: Bits = b(8, 8);
        pub(crate) const context_update_tile_id: Bits = b(16, 16);
    }

    /// `CUVIDAV1PICPARAMS`'s CDEF byte.
    pub(crate) mod av1_cdef {
        use super::{Bits, b};
        pub(crate) const cdef_damping_minus_3: Bits = b(0, 2);
        pub(crate) const cdef_bits: Bits = b(2, 2);
    }

    /// `CUVIDAV1PICPARAMS`'s skip mode byte.
    pub(crate) mod av1_skip_mode {
        use super::{Bits, b};
        pub(crate) const SkipModeFrame0: Bits = b(0, 4);
        pub(crate) const SkipModeFrame1: Bits = b(4, 4);
    }

    /// `CUVIDAV1PICPARAMS`'s segmentation byte.
    pub(crate) mod av1_segmentation {
        use super::{Bits, b};
        pub(crate) const segmentation_enabled: Bits = b(0, 1);
        pub(crate) const segmentation_update_map: Bits = b(1, 1);
        pub(crate) const segmentation_update_data: Bits = b(2, 1);
        pub(crate) const segmentation_temporal_update: Bits = b(3, 1);
    }

    /// `CUVIDAV1PICPARAMS`'s loop filter byte.
    pub(crate) mod av1_loop_filter {
        use super::{Bits, b};
        pub(crate) const loop_filter_delta_enabled: Bits = b(0, 1);
        pub(crate) const loop_filter_delta_update: Bits = b(1, 1);
        pub(crate) const delta_lf_present: Bits = b(2, 1);
        pub(crate) const delta_lf_res: Bits = b(3, 2);
        pub(crate) const delta_lf_multi: Bits = b(5, 1);
    }

    /// `CUVIDAV1PICPARAMS`'s layer byte.
    pub(crate) mod av1_layer {
        use super::{Bits, b};
        pub(crate) const temporal_layer_id: Bits = b(0, 4);
        pub(crate) const spatial_layer_id: Bits = b(4, 4);
    }

    /// `CUVIDAV1PICPARAMS::global_motion`'s flags byte.
    pub(crate) mod av1_global_motion {
        use super::{Bits, b};
        pub(crate) const invalid: Bits = b(0, 1);
        pub(crate) const wmtype: Bits = b(1, 2);
    }

    /// `CUVIDAV1PICPARAMS`'s film grain word (an `unsigned short`).
    pub(crate) mod av1_film_grain {
        use super::{Bits, b};
        pub(crate) const apply_grain: Bits = b(0, 1);
        pub(crate) const overlap_flag: Bits = b(1, 1);
        pub(crate) const scaling_shift_minus8: Bits = b(2, 2);
        pub(crate) const chroma_scaling_from_luma: Bits = b(4, 1);
        pub(crate) const ar_coeff_lag: Bits = b(5, 2);
        pub(crate) const ar_coeff_shift_minus6: Bits = b(7, 2);
        pub(crate) const grain_scale_shift: Bits = b(9, 2);
        pub(crate) const clip_to_restricted_range: Bits = b(11, 1);
    }
}

functions! {
    /// libnvcuvid's decoder functions.
    pub(crate) struct NvdecFunctions {
        cuvidGetDecoderCaps: fn(*mut CUVIDDECODECAPS) -> CUresult = c"cuvidGetDecoderCaps";
        cuvidCreateDecoder: fn(*mut CUvideodecoder, *mut CUVIDDECODECREATEINFO) -> CUresult
            = c"cuvidCreateDecoder";
        cuvidDestroyDecoder: fn(CUvideodecoder) -> CUresult = c"cuvidDestroyDecoder";
        cuvidDecodePicture: fn(CUvideodecoder, *mut CUVIDPICPARAMS) -> CUresult
            = c"cuvidDecodePicture";
        cuvidGetDecodeStatus: fn(CUvideodecoder, c_int, *mut CUVIDGETDECODESTATUS) -> CUresult
            = c"cuvidGetDecodeStatus";
        cuvidMapVideoFrame64: fn(
            CUvideodecoder,
            c_int,
            *mut c_ulonglong,
            *mut c_uint,
            *mut CUVIDPROCPARAMS,
        ) -> CUresult = c"cuvidMapVideoFrame64";
        cuvidUnmapVideoFrame64: fn(CUvideodecoder, c_ulonglong) -> CUresult
            = c"cuvidUnmapVideoFrame64";
        cuvidReconfigureDecoder: fn(CUvideodecoder, *mut CUVIDRECONFIGUREDECODERINFO) -> CUresult
            = c"cuvidReconfigureDecoder";
    }
}

// SAFETY: each is repr(C) and holds only integers, raw pointers, and
// arrays, structs and unions of those (`Zeroed`'s contract).
unsafe impl Zeroed for CUVIDDECODECAPS {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDDECODECREATEINFO {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDPICPARAMS {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDH264PICPARAMS {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDHEVCPICPARAMS {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDVP9PICPARAMS {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDAV1PICPARAMS {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDPROCPARAMS {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDGETDECODESTATUS {}
// SAFETY: as above.
unsafe impl Zeroed for CUVIDRECONFIGUREDECODERINFO {}

// The sizes ffnvcodec 13.1.15.0's header gives on x86-64 Linux
// (2026-10-24); `crate::layout`'s test checks every offset and bit-field
// too.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<CUVIDDECODECAPS>() == 88);
    assert!(size_of::<CUVIDDECODECREATEINFO>() == 176);
    assert!(size_of::<CUVIDH264PICPARAMS>() == 984);
    assert!(size_of::<CUVIDHEVCPICPARAMS>() == 1484);
    assert!(size_of::<CUVIDVP9PICPARAMS>() == 220);
    assert!(size_of::<CUVIDAV1PICPARAMS>() == 1024);
    assert!(size_of::<CUVIDPICPARAMS>() == 4280);
    assert!(size_of::<CUVIDPROCPARAMS>() == 264);
    assert!(size_of::<CUVIDGETDECODESTATUS>() == 192);
    assert!(size_of::<CUVIDRECONFIGUREDECODERINFO>() == 128);
};

#[cfg(test)]
mod tests {
    use super::bits::{av1_tiling, h264, vp9_frame};

    #[test]
    fn a_bit_field_is_set_alone_and_cut_to_its_width() {
        // `num_tile_cols : 8`, `num_tile_rows : 8`, at the word's bottom.
        let word = av1_tiling::num_tile_rows.put(av1_tiling::num_tile_cols.put(0, 4), 2);
        assert_eq!(word, 0x0204);
        assert_eq!(av1_tiling::num_tile_cols.put(word, 7), 0x0207);
        // A value wider than its field keeps to it.
        assert_eq!(h264::mb_adaptive_frame_field_flag.put(!3_u32, 5), !2);
        assert_eq!(
            vp9_frame::profile.mask().count_ones(),
            vp9_frame::profile.width
        );
    }
}
