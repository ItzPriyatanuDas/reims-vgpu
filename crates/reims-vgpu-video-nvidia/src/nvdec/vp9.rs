//! VP9 on NVDEC: the codec crate's decoder over an accelerator that fills
//! `CUVIDVP9PICPARAMS` from the frame's uncompressed header, with what it
//! inherits in force, and hands NVDEC the frame from its uncompressed
//! header to its last tile. NVDEC reads the compressed header and the
//! tiles itself, and keeps the probability contexts, as ffmpeg's
//! nvdec_vp9.c has it: `frameTagSize` is the uncompressed header's size
//! and `offsetToDctParts` the compressed header's.
//!
//! The fields follow libvpx where VP9's specification numbers otherwise,
//! as the VA-API backend's do: `mcomp_filter_type` in libvpx's
//! `INTERP_FILTER` order, which swaps the specification's first two.
//!
//! An NVDEC decoder's surfaces all have one size, the key frame's. A frame
//! of another size, which an inter or intra-only frame may bring, is
//! decoded into those surfaces when it fits in them, and refused when it
//! does not. Surfaces made larger, at the format description's size, do
//! not help: a stream of spatial layers, whose frames are larger than its
//! key frame, then decodes wrongly on NVDEC from its second top-layer
//! frame on, and drifts further from libvpx's decode with each frame.

use std::ffi::{c_int, c_uint};
use std::sync::Arc;

use reims_vgpu_video_codec::vp9::header::{FrameType, InterpolationFilter, NUM_REF_FRAMES};
use reims_vgpu_video_codec::vp9::{Accelerator, FrameParams, StreamFormat};
use reims_vgpu_video_core::sync::Mutex;

use super::decoder::{Decoder, DecoderSpec, Picture, SPARE_SURFACES, Slices, surface_count};
use super::ffi::{self, Zeroed};
use super::{ChromaFormat, NvdecCodec};
use crate::cuda::Gpu;
use crate::error::Error;
use crate::wait::Deadline;

/// A reference slot NVDEC is told is empty.
const NO_REFERENCE: u8 = 0xff;

/// The NVDEC half of the VP9 decoder.
pub(crate) struct NvdecVp9 {
    gpu: Arc<Gpu>,
    /// The decoder of the last key frame's format and size.
    decoder: Option<Decoder>,
    /// The deadline of the decode call in hand ([`NvdecVp9::start_call`]),
    /// which its pictures' decodes wait until.
    call: Mutex<Deadline>,
}

impl NvdecVp9 {
    pub(crate) fn new(gpu: Arc<Gpu>) -> Self {
        Self {
            gpu,
            decoder: None,
            call: Mutex::new(Deadline::default()),
        }
    }

    /// Starts a decode call: its deadline, which the decodes of the
    /// pictures it decodes wait until, and the read backs of the frames it
    /// gives share (`crate::wait::Deadline`).
    pub(crate) fn start_call(&self) -> Deadline {
        let deadline = Deadline::default();
        *self.call.lock() = deadline.clone();
        deadline
    }

    /// `Err` where the decoder of the last key frame is stalled, as each
    /// of its later decodes fails ([`Error::Stalled`]).
    pub(crate) fn check_not_stalled(&self) -> Result<(), Error> {
        self.decoder
            .as_ref()
            .map_or(Ok(()), Decoder::check_not_stalled)
    }

    fn decoder(&self) -> Result<&Decoder, Error> {
        self.decoder
            .as_ref()
            .ok_or_else(|| Error::Unsupported("decoding before a key frame".into()))
    }
}

/// The bit depth of a stream this decoder takes, or why it refuses it:
/// Profile 0 at 8 bits and Profile 2 at 10, 4:2:0.
pub(crate) fn check_format(profile: u8, bit_depth: u8, subsampled_420: bool) -> Result<u32, Error> {
    match (profile, bit_depth) {
        _ if profile == 1 || profile == 3 => Err(Error::Unsupported(format!(
            "VP9 Profile {profile} (4:2:2, 4:4:0 and 4:4:4)"
        ))),
        _ if !subsampled_420 => Err(Error::Unsupported("VP9 chroma other than 4:2:0".into())),
        (0, 8) | (2, 10) => Ok(u32::from(bit_depth)),
        _ => Err(Error::Unsupported(format!(
            "VP9 Profile {profile} at {bit_depth} bits"
        ))),
    }
}

/// The decoder a key frame of `width` by `height` at `bit_depth` starts.
pub(crate) fn spec(bit_depth: u32, width: u32, height: u32) -> DecoderSpec {
    DecoderSpec {
        codec: NvdecCodec::Vp9,
        chroma: ChromaFormat::Yuv420,
        bit_depth,
        width: width.next_multiple_of(2),
        height: height.next_multiple_of(2),
        surfaces: surface_count(NUM_REF_FRAMES + 1 + SPARE_SURFACES),
    }
}

impl Accelerator for NvdecVp9 {
    type Picture = Picture;
    type Error = Error;

    fn configure(&mut self, format: &StreamFormat, width: u32, height: u32) -> Result<(), Error> {
        self.decoder = None;
        let bit_depth = check_format(
            format.profile,
            format.bit_depth,
            format.subsampling_x && format.subsampling_y,
        )?;
        self.decoder = Some(Decoder::new(
            Arc::clone(&self.gpu),
            &spec(bit_depth, width, height),
        )?);
        Ok(())
    }

    fn new_picture(&mut self, width: u32, height: u32) -> Result<Picture, Error> {
        let decoder = self.decoder()?;
        let (surface_width, surface_height) = decoder.size();
        if width > surface_width || height > surface_height {
            return Err(Error::Unsupported(format!(
                "a VP9 frame of {width}x{height} after a key frame of \
                 {surface_width}x{surface_height}: NVDEC's surfaces are the key frame's, \
                 and NVDEC decodes spatial layers wrongly"
            )));
        }
        // At its own size, rounded up to even as the surfaces are: mapped
        // at its odd size, a 10-bit frame comes out wrong (p2-643x363,
        // 2026-10-24).
        decoder.picture_of_size((width.next_multiple_of(2), height.next_multiple_of(2)))
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, Picture>) -> Result<(), Error> {
        let mut pic = picture_parameters(p)?;
        let deadline = self.call.lock().clone();
        self.decoder()?
            .decode_by(&mut pic, p.data, Slices::Starts(&[0]), &deadline)
    }
}

/// `mcomp_filter_type`: libvpx's `INTERP_FILTER` numbering.
fn mcomp_filter_type(filter: InterpolationFilter) -> c_uint {
    match filter {
        InterpolationFilter::EightTap => 0,
        InterpolationFilter::EightTapSmooth => 1,
        InterpolationFilter::EightTapSharp => 2,
        InterpolationFilter::Bilinear => 3,
        InterpolationFilter::Switchable => 4,
    }
}

/// The frame's `CUVIDPICPARAMS`, all but its bitstream.
fn picture_parameters(p: &FrameParams<'_, Picture>) -> Result<ffi::CUVIDPICPARAMS, Error> {
    use ffi::bits::{vp9_frame as f, vp9_segment as s};
    let h = p.header;
    let inter = !h.is_intra();
    let mut out = ffi::CUVIDPICPARAMS::zeroed();
    out.PicWidthInMbs = c_int::try_from(h.width.div_ceil(16)).unwrap_or(c_int::MAX);
    out.FrameHeightInMbs = c_int::try_from(h.height.div_ceil(16)).unwrap_or(c_int::MAX);
    out.CurrPicIdx = p.picture.index();
    out.ref_pic_flag = c_int::from(h.refresh_frame_flags != 0);
    out.intra_pic_flag = c_int::from(!inter);

    let mut v = ffi::CUVIDVP9PICPARAMS::zeroed();
    v.width = h.width;
    v.height = h.height;
    let slot = |reference: u8| {
        p.references
            .get(usize::from(reference))
            .copied()
            .flatten()
            .map_or(NO_REFERENCE, |picture| {
                u8::try_from(picture.index()).unwrap_or(NO_REFERENCE)
            })
    };
    if inter {
        let [last, golden, alt] = h.ref_frame_idx;
        v.LastRefIdx = slot(last);
        v.GoldenRefIdx = slot(golden);
        v.AltRefIdx = slot(alt);
        v.activeRefIdx = h.ref_frame_idx.map(c_uint::from);
        for (bias, &sign) in v.refFrameSignBias[1..]
            .iter_mut()
            .zip(&h.ref_frame_sign_bias)
        {
            *bias = u8::from(sign);
        }
        v.mcomp_filter_type = mcomp_filter_type(h.interpolation_filter);
    } else {
        v.LastRefIdx = NO_REFERENCE;
        v.GoldenRefIdx = NO_REFERENCE;
        v.AltRefIdx = NO_REFERENCE;
    }
    v.colorSpace = h.color.color_space;
    let flag = u32::from;
    let mut word = f::profile.put(0, u32::from(h.profile));
    word = f::frameContextIdx.put(word, u32::from(h.frame_context_idx));
    word = f::frameType.put(word, flag(h.frame_type == FrameType::NonKey));
    word = f::showFrame.put(word, flag(h.show_frame));
    word = f::errorResilient.put(word, flag(h.error_resilient_mode));
    word = f::frameParallelDecoding.put(word, flag(h.frame_parallel_decoding_mode));
    word = f::subSamplingX.put(word, flag(h.color.subsampling_x));
    word = f::subSamplingY.put(word, flag(h.color.subsampling_y));
    word = f::intraOnly.put(word, flag(h.intra_only));
    word = f::allow_high_precision_mv.put(word, flag(inter && h.allow_high_precision_mv));
    word = f::refreshEntropyProbs.put(word, flag(h.refresh_frame_context));
    v.frame_word = u16::try_from(word).unwrap_or(0);
    v.bitDepthMinus8Luma = h.color.bit_depth.saturating_sub(8);
    v.bitDepthMinus8Chroma = h.color.bit_depth.saturating_sub(8);

    let lf = &h.loop_filter;
    v.loopFilterLevel = lf.level;
    v.loopFilterSharpness = lf.sharpness;
    v.modeRefLfEnabled = u8::from(lf.delta_enabled);
    v.mbRefLfDelta = lf.ref_deltas.map(|d| i32::from(d).cast_unsigned());
    v.mbModeLfDelta = lf.mode_deltas.map(|d| i32::from(d).cast_unsigned());
    v.log2_tile_columns = h.tile_info.cols_log2;
    v.log2_tile_rows = h.tile_info.rows_log2;

    let seg = &h.segmentation;
    let mut segment_word = s::segmentEnabled.put(0, flag(seg.enabled));
    segment_word = s::segmentMapUpdate.put(segment_word, flag(seg.update_map));
    segment_word = s::segmentMapTemporalUpdate.put(segment_word, flag(seg.temporal_update));
    segment_word = s::segmentFeatureMode.put(segment_word, flag(seg.abs_or_delta_update));
    v.segment_word = u8::try_from(segment_word).unwrap_or(0);
    v.segmentFeatureEnable = seg.feature_enabled.map(|features| features.map(u8::from));
    v.segmentFeatureData = seg.feature_data;
    v.mb_segment_tree_probs = seg.tree_probs;
    // 255 each without a temporal update, as libvpx's
    // setup_segmentation() sets them.
    v.segment_pred_probs = if seg.temporal_update {
        seg.pred_probs
    } else {
        [255; 3]
    };

    let q = &h.quantization;
    v.qpYAc = c_int::from(q.base_q_idx);
    v.qpYDc = c_int::from(q.delta_q_y_dc);
    v.qpChDc = c_int::from(q.delta_q_uv_dc);
    v.qpChAc = c_int::from(q.delta_q_uv_ac);
    v.resetFrameContext = c_uint::from(h.reset_frame_context);
    v.frameTagSize = c_uint::try_from(h.uncompressed_header_size)
        .map_err(|_| Error::Unsupported("an uncompressed header over 4 GiB".into()))?;
    v.offsetToDctParts = c_uint::from(h.header_size_in_bytes);
    out.CodecSpecific.vp9 = v;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_0_at_8_bits_and_profile_2_at_10_are_taken_at_4_2_0() {
        assert_eq!(check_format(0, 8, true).ok(), Some(8));
        assert_eq!(check_format(2, 10, true).ok(), Some(10));
        for (profile, bit_depth, subsampled_420) in [
            (1, 8, false),
            (3, 10, false),
            (0, 8, false),
            (0, 10, true),
            (2, 12, true),
        ] {
            assert!(
                matches!(
                    check_format(profile, bit_depth, subsampled_420),
                    Err(Error::Unsupported(_))
                ),
                "Profile {profile} at {bit_depth} bits"
            );
        }
    }
}
