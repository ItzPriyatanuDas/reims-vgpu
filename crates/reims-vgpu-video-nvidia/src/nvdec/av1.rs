//! AV1 on NVDEC: the codec crate's decoder over an accelerator that fills
//! `CUVIDAV1PICPARAMS` from the sequence header and the frame header, with
//! every value a frame inherits in force, and hands NVDEC the frame's tile
//! groups with where each tile starts and ends, as ffmpeg's nvdec_av1.c
//! does. NVIDIA's own parser is not used.
//!
//! The conventions it follows, as dynlink_cuviddec.h and ffmpeg's
//! nvdec_av1.c have them, and as NVIDIA's own parser fills the structure
//! (ffmpeg rounds the macroblock counts up and gives the references their
//! upscaled widths, which decode odd sizes, resized frames and superres
//! wrongly):
//! - `width` is the upscaled width, and `PicWidthInMbs` and
//!   `FrameHeightInMbs` count whole macroblocks, rounding down;
//! - references are named by surface index: `ref_frame_map`, each of the
//!   seven references with its coded size (before superres's upscaling),
//!   and `primary_ref_frame` too, which is the primary reference's surface
//!   (255 for none), not its number;
//! - `ref_pic_flag` is set for a frame some slot keeps;
//! - the decoder is made at the frame's own size, odd too, and each frame
//!   is mapped at its own size;
//! - `lr_type` is `FrameRestorationType`, and `lr_unit_size` 0 for 32 up
//!   to 3 for 256;
//! - each CDEF strength byte holds the primary strength in its low four
//!   bits and the secondary, as coded, in its high four;
//! - the bitstream is the tile groups' data one after another, and
//!   `pSliceDataOffsets` holds two offsets a tile, its start and its end,
//!   in the frame's tile order;
//! - with film grain applied, `CurrPicIdx` takes the frame with its grain
//!   and `decodePicIdx` the frame as later frames predict from it.

use std::ffi::{c_int, c_short};
use std::sync::Arc;

use reims_vgpu_video_codec::av1::frame::{FrameHeader, FrameType, NUM_REF_FRAMES};
use reims_vgpu_video_codec::av1::sequence::SequenceHeader;
use reims_vgpu_video_codec::av1::{Accelerator, FrameParams};

use super::NvdecCodec;
use super::decoder::{Decoder, DecoderSpec, Picture, SPARE_SURFACES, Slices, surface_count};
use super::ffi::{self, Zeroed};
use super::{ChromaFormat, decode_capabilities};
use crate::cuda::Gpu;
use crate::error::Error;

/// A surface NVDEC is told is not there.
const NO_PICTURE: u8 = 0xff;

/// `PRIMARY_REF_NONE`.
const PRIMARY_REF_NONE: u8 = 7;

/// `SUPERRES_DENOM_MIN`: what `coded_denom` counts from.
const SUPERRES_DENOM_MIN: u8 = 9;

/// The NVDEC half of the AV1 decoder.
pub(crate) struct NvdecAv1 {
    gpu: Arc<Gpu>,
    /// The decoder of the sequence's frames.
    decoder: Option<Decoder>,
    /// `FrameWidth` of the frame each surface was last decoded at, by
    /// surface index: a reference's coded width.
    coded_widths: Vec<u32>,
    /// Whether NVDEC decoded the frame each surface holds wrongly, by
    /// surface index ([`decodes_wrongly`]).
    wrong: Vec<bool>,
}

/// Ours: whether NVDEC decodes an AV1 frame of `width` by `height` wrongly.
/// A side of 4n+3 (427, 643, 363, 1283) gave wrong blocks in every frame at
/// 8 and 10 bits, with no error, through NVIDIA's own parser too, where
/// every other size was bit-exact (RTX 5080 Laptop GPU, driver 615.71.09,
/// 2026-10-24). The session refuses such a frame rather than show it
/// (`docs/video/bugs.md`, U7).
pub(crate) fn decodes_wrongly(width: u32, height: u32) -> bool {
    width % 4 == 3 || height % 4 == 3
}

impl NvdecAv1 {
    pub(crate) fn new(gpu: Arc<Gpu>) -> Self {
        Self {
            gpu,
            decoder: None,
            coded_widths: Vec::new(),
            wrong: Vec::new(),
        }
    }

    /// Whether `picture` holds a frame NVDEC decoded wrongly: one of a size
    /// [`decodes_wrongly`] names, or one predicted from such a frame.
    pub(crate) fn decoded_wrongly(&self, picture: &Picture) -> bool {
        usize::try_from(picture.index())
            .ok()
            .and_then(|index| self.wrong.get(index))
            .is_some_and(|wrong| *wrong)
    }

    fn decoder(&self) -> Result<&Decoder, Error> {
        self.decoder
            .as_ref()
            .ok_or_else(|| Error::Unsupported("decoding before a sequence header".into()))
    }
}

/// The bit depth of a stream this decoder takes, or why it refuses it:
/// Main (`seq_profile` 0) at 8 or 10 bits, 4:2:0 or monochrome.
pub(crate) fn check_format(
    seq_profile: u8,
    bit_depth: u8,
    mono_chrome: bool,
    subsampled_420: bool,
) -> Result<u32, Error> {
    match seq_profile {
        0 => {}
        1 => return Err(Error::Unsupported("AV1 High profile (4:4:4)".into())),
        _ => return Err(Error::Unsupported("AV1 Professional profile".into())),
    }
    if !mono_chrome && !subsampled_420 {
        return Err(Error::Unsupported("AV1 chroma other than 4:2:0".into()));
    }
    match bit_depth {
        8 | 10 => Ok(u32::from(bit_depth)),
        other => Err(Error::Unsupported(format!("AV1 at {other} bits"))),
    }
}

/// The decoder `sequence`'s frames need: its surfaces hold the largest
/// frame the sequence allows, when NVDEC takes that, and else `first`, the
/// first frame's size (or the description's, before a frame). An encoder
/// may allow far more than it codes (SVT-AV1 with switch frames allows
/// 16384x8704 for a 720p stream, as the VA-API backend measured), and a
/// later frame larger than the surfaces is refused when it comes.
pub(crate) fn spec(
    gpu: &Gpu,
    sequence: &SequenceHeader,
    first: (u32, u32),
) -> Result<DecoderSpec, Error> {
    let c = &sequence.color;
    let bit_depth = check_format(
        sequence.seq_profile,
        c.bit_depth,
        c.mono_chrome,
        c.subsampling_x && c.subsampling_y,
    )?;
    let chroma = if c.mono_chrome {
        ChromaFormat::Monochrome
    } else {
        ChromaFormat::Yuv420
    };
    let caps = decode_capabilities(gpu, NvdecCodec::Av1, chroma, bit_depth)?;
    let fits = |(width, height): (u32, u32)| width <= caps.max_width && height <= caps.max_height;
    let largest = sequence.max_frame_size();
    let (width, height) = if fits(largest) { largest } else { first };
    // Film grain doubles what is kept: each slot may hold a frame with its
    // grain beside the frame as decoded.
    let pictures = NUM_REF_FRAMES + 1 + SPARE_SURFACES;
    let pictures = if sequence.film_grain_params_present {
        pictures * 2
    } else {
        pictures
    };
    Ok(DecoderSpec {
        codec: NvdecCodec::Av1,
        chroma,
        bit_depth,
        // The frame's own size, odd too, as NVIDIA's parser asks for it.
        width,
        height,
        surfaces: surface_count(pictures),
    })
}

impl Accelerator for NvdecAv1 {
    type Picture = Picture;
    type Error = Error;

    fn configure(&mut self, sequence: &SequenceHeader, first: &FrameHeader) -> Result<(), Error> {
        self.decoder = None;
        let spec = spec(
            &self.gpu,
            sequence,
            (first.upscaled_width, first.frame_height),
        )?;
        self.decoder = Some(Decoder::new(Arc::clone(&self.gpu), &spec)?);
        self.coded_widths = vec![0; usize::try_from(spec.surfaces).unwrap_or(0)];
        self.wrong = vec![false; self.coded_widths.len()];
        Ok(())
    }

    fn new_picture(&mut self) -> Result<Picture, Error> {
        self.decoder()?.picture()
    }

    fn new_display_picture(&mut self) -> Result<Picture, Error> {
        self.decoder()?.picture()
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, Picture>) -> Result<(), Error> {
        let decoder = self.decoder()?;
        let (width, height) = (p.header.upscaled_width, p.header.frame_height);
        let (surface_width, surface_height) = decoder.size();
        if width > surface_width || height > surface_height {
            return Err(Error::Unsupported(format!(
                "an AV1 frame of {width}x{height}, larger than the decoder's \
                 {surface_width}x{surface_height}"
            )));
        }
        // Mapped at its own size, odd too: an odd-sized frame mapped at its
        // size rounded up to even comes out wrong, unlike VP9's
        // (aom-641x361, 2026-10-24).
        p.picture.set_size((width, height));
        if let Some(display) = p.display {
            display.set_size((width, height));
        }
        let mut bitstream = Vec::with_capacity(p.tile_groups.iter().map(|g| g.data.len()).sum());
        // At most 64 by 64 tiles: the codec crate's frame header holds
        // them to the specification's MAX_TILE_COLS and MAX_TILE_ROWS.
        let tiles = usize::try_from(p.header.tile_info.num_tiles()).unwrap_or(0);
        let mut offsets = vec![[0u32; 2]; tiles];
        let columns = p.header.tile_info.cols;
        let offset = |v: Option<usize>| {
            v.and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| Error::Unsupported("a frame over 4 GiB".into()))
        };
        for group in p.tile_groups {
            let base = bitstream.len();
            bitstream.extend_from_slice(group.data);
            for tile in &group.tiles {
                let number = (tile.row.checked_mul(columns))
                    .and_then(|first| first.checked_add(tile.column))
                    .and_then(|number| usize::try_from(number).ok());
                let slot = number
                    .and_then(|number| offsets.get_mut(number))
                    .ok_or_else(|| {
                        Error::Bitstream(format!(
                            "a tile at row {} column {}, of {tiles} tiles",
                            tile.row, tile.column
                        ))
                    })?;
                let start = base.checked_add(tile.offset);
                *slot = [
                    offset(start)?,
                    offset(start.and_then(|start| start.checked_add(tile.size)))?,
                ];
            }
        }
        let mut pic = picture_parameters(p, &self.coded_widths);
        decoder.decode(&mut pic, &bitstream, Slices::Tiles(&offsets))?;
        if let Some(slot) = usize::try_from(p.picture.index())
            .ok()
            .and_then(|index| self.coded_widths.get_mut(index))
        {
            *slot = p.header.frame_width;
        }
        // An inter frame predicted from a wrong frame is wrong too.
        let wrong = decodes_wrongly(width, height)
            || (!p.header.frame_type.is_intra()
                && p.header.ref_frame_idx.iter().any(|&slot| {
                    p.references
                        .get(usize::from(slot))
                        .copied()
                        .flatten()
                        .is_some_and(|reference| self.decoded_wrongly(reference))
                }));
        for picture in std::iter::once(p.picture).chain(p.display) {
            if let Some(slot) = usize::try_from(picture.index())
                .ok()
                .and_then(|index| self.wrong.get_mut(index))
            {
                *slot = wrong;
            }
        }
        Ok(())
    }
}

fn narrow8(value: u32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn narrow16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// The surface of a picture, as `ref_frame_map` names it.
fn surface(picture: Option<&Picture>) -> u8 {
    picture.map_or(NO_PICTURE, |picture| {
        u8::try_from(picture.index()).unwrap_or(NO_PICTURE)
    })
}

/// The frame's `CUVIDPICPARAMS`, all but its bitstream. `coded_widths`
/// holds each surface's `FrameWidth`.
fn picture_parameters(p: &FrameParams<'_, Picture>, coded_widths: &[u32]) -> ffi::CUVIDPICPARAMS {
    use ffi::bits::{
        av1_cdef as cd, av1_film_grain as fg, av1_frame as fr, av1_global_motion as gm,
        av1_layer as la, av1_loop_filter as lf, av1_segmentation as sg, av1_sequence as sq,
        av1_skip_mode as sk, av1_tiling as ti,
    };
    let (s, h) = (p.sequence, p.header);
    let c = &s.color;
    let flag = u32::from;
    let apply_grain = h.film_grain.apply_grain && p.display.is_some();
    let shown = p.display.unwrap_or(p.picture);

    let mut out = ffi::CUVIDPICPARAMS::zeroed();
    // Whole macroblocks, as NVIDIA's parser counts them: rounding up
    // decodes an odd-sized frame wrongly (aom-641x361, 2026-10-24).
    out.PicWidthInMbs = c_int::try_from(h.upscaled_width / 16).unwrap_or(c_int::MAX);
    out.FrameHeightInMbs = c_int::try_from(h.frame_height / 16).unwrap_or(c_int::MAX);
    out.CurrPicIdx = shown.index();
    out.ref_pic_flag = c_int::from(h.refresh_frame_flags != 0);
    out.intra_pic_flag = c_int::from(h.is_intra());

    let mut a = ffi::CUVIDAV1PICPARAMS::zeroed();
    a.width = h.upscaled_width;
    a.height = h.frame_height;
    a.frame_offset = h.order_hint;
    a.decodePicIdx = p.picture.index();

    let mut word = sq::profile.put(0, u32::from(s.seq_profile));
    word = sq::use_128x128_superblock.put(word, flag(s.use_128x128_superblock));
    word = sq::subsampling_x.put(word, flag(c.subsampling_x));
    word = sq::subsampling_y.put(word, flag(c.subsampling_y));
    word = sq::mono_chrome.put(word, flag(c.mono_chrome));
    word = sq::bit_depth_minus8.put(word, u32::from(c.bit_depth.saturating_sub(8)));
    word = sq::enable_filter_intra.put(word, flag(s.enable_filter_intra));
    word = sq::enable_intra_edge_filter.put(word, flag(s.enable_intra_edge_filter));
    word = sq::enable_interintra_compound.put(word, flag(s.enable_interintra_compound));
    word = sq::enable_masked_compound.put(word, flag(s.enable_masked_compound));
    word = sq::enable_dual_filter.put(word, flag(s.enable_dual_filter));
    word = sq::enable_order_hint.put(word, flag(s.enable_order_hint));
    word = sq::order_hint_bits_minus1.put(word, u32::from(s.order_hint_bits.saturating_sub(1)));
    word = sq::enable_jnt_comp.put(word, flag(s.enable_jnt_comp));
    word = sq::enable_superres.put(word, flag(s.enable_superres));
    word = sq::enable_cdef.put(word, flag(s.enable_cdef));
    word = sq::enable_restoration.put(word, flag(s.enable_restoration));
    word = sq::enable_fgs.put(word, flag(s.film_grain_params_present));
    a.sequence_word = word;

    let coded_denom = if h.use_superres {
        h.superres_denom.saturating_sub(SUPERRES_DENOM_MIN)
    } else {
        0
    };
    let frame_type = match h.frame_type {
        FrameType::Key => 0,
        FrameType::Inter => 1,
        FrameType::IntraOnly => 2,
        FrameType::Switch => 3,
    };
    let mut word = fr::frame_type.put(0, frame_type);
    word = fr::show_frame.put(word, flag(h.show_frame));
    word = fr::disable_cdf_update.put(word, flag(h.disable_cdf_update));
    word = fr::allow_screen_content_tools.put(word, flag(h.allow_screen_content_tools));
    word = fr::force_integer_mv.put(word, flag(h.force_integer_mv));
    word = fr::coded_denom.put(word, u32::from(coded_denom));
    word = fr::allow_intrabc.put(word, flag(h.allow_intrabc));
    word = fr::allow_high_precision_mv.put(word, flag(h.allow_high_precision_mv));
    word = fr::interp_filter.put(word, u32::from(h.interpolation_filter));
    word = fr::switchable_motion_mode.put(word, flag(h.is_motion_mode_switchable));
    word = fr::use_ref_frame_mvs.put(word, flag(h.use_ref_frame_mvs));
    word = fr::disable_frame_end_update_cdf.put(word, flag(h.disable_frame_end_update_cdf));
    word = fr::delta_q_present.put(word, flag(h.delta_q_present));
    word = fr::delta_q_res.put(word, u32::from(h.delta_q_res));
    word = fr::using_qmatrix.put(word, flag(h.quantization.using_qmatrix));
    word = fr::coded_lossless.put(word, flag(h.coded_lossless));
    word = fr::use_superres.put(word, flag(h.use_superres));
    word = fr::tx_mode.put(word, u32::from(h.tx_mode));
    word = fr::reference_mode.put(word, flag(h.reference_select));
    word = fr::allow_warped_motion.put(word, flag(h.allow_warped_motion));
    word = fr::reduced_tx_set.put(word, flag(h.reduced_tx_set));
    word = fr::skip_mode.put(word, flag(h.skip_mode_present));
    a.frame_word = word;

    let t = &h.tile_info;
    let mut word = ti::num_tile_cols.put(0, t.cols);
    word = ti::num_tile_rows.put(word, t.rows);
    word = ti::context_update_tile_id.put(word, t.context_update_tile_id);
    a.tiling_word = word;
    for (slot, &width) in a.tile_widths.iter_mut().zip(&t.width_in_sbs) {
        *slot = narrow16(width);
    }
    for (slot, &height) in a.tile_heights.iter_mut().zip(&t.height_in_sbs) {
        *slot = narrow16(height);
    }

    let cdef = &h.cdef;
    a.cdef_word = narrow8(
        cd::cdef_damping_minus_3.put(0, u32::from(cdef.damping_minus_3))
            | cd::cdef_bits.put(0, u32::from(cdef.bits)),
    );
    let strengths = |primary: &[u8; 8], secondary: &[u8; 8]| -> [u8; 8] {
        core::array::from_fn(|i| (primary[i] & 0x0f) | (secondary[i] << 4))
    };
    a.cdef_y_strength = strengths(&cdef.y_pri_strength, &cdef.y_sec_strength);
    a.cdef_uv_strength = strengths(&cdef.uv_pri_strength, &cdef.uv_sec_strength);
    a.skip_mode_word = narrow8(
        sk::SkipModeFrame0.put(0, u32::from(h.skip_mode_frame[0]))
            | sk::SkipModeFrame1.put(0, u32::from(h.skip_mode_frame[1])),
    );

    let q = &h.quantization;
    a.base_qindex = q.base_q_idx;
    a.qp_y_dc_delta_q = q.delta_q_y_dc;
    a.qp_u_dc_delta_q = q.delta_q_u_dc;
    a.qp_v_dc_delta_q = q.delta_q_v_dc;
    a.qp_u_ac_delta_q = q.delta_q_u_ac;
    a.qp_v_ac_delta_q = q.delta_q_v_ac;
    a.qm_y = q.qm_y;
    a.qm_u = q.qm_u;
    a.qm_v = q.qm_v;

    let seg = &h.segmentation;
    a.segmentation_word = narrow8(
        sg::segmentation_enabled.put(0, flag(seg.enabled))
            | sg::segmentation_update_map.put(0, flag(seg.update_map))
            | sg::segmentation_update_data.put(0, flag(seg.update_data))
            | sg::segmentation_temporal_update.put(0, flag(seg.temporal_update)),
    );
    a.segmentation_feature_data = seg.feature_data;
    a.segmentation_feature_mask = seg.feature_mask;

    let l = &h.loop_filter;
    a.loop_filter_level = [l.level[0], l.level[1]];
    a.loop_filter_level_u = l.level[2];
    a.loop_filter_level_v = l.level[3];
    a.loop_filter_sharpness = l.sharpness;
    a.loop_filter_ref_deltas = l.ref_deltas;
    a.loop_filter_mode_deltas = l.mode_deltas;
    a.loop_filter_word = narrow8(
        lf::loop_filter_delta_enabled.put(0, flag(l.delta_enabled))
            | lf::loop_filter_delta_update.put(0, flag(l.delta_update))
            | lf::delta_lf_present.put(0, flag(h.delta_lf_present))
            | lf::delta_lf_res.put(0, u32::from(h.delta_lf_res))
            | lf::delta_lf_multi.put(0, flag(h.delta_lf_multi)),
    );

    let lr = &h.loop_restoration;
    a.lr_type = lr.frame_restoration_type;
    // 64 << lr_unit_shift for luma, as 0 for 32 up to 3 for 256.
    let luma = 1 + lr.lr_unit_shift;
    a.lr_unit_size = [
        luma,
        luma.saturating_sub(lr.lr_uv_shift),
        luma.saturating_sub(lr.lr_uv_shift),
    ];

    a.ref_frame_map = p.references.map(surface);
    a.primary_ref_frame = if h.primary_ref_frame == PRIMARY_REF_NONE {
        NO_PICTURE
    } else {
        h.ref_frame_idx
            .get(usize::from(h.primary_ref_frame))
            .and_then(|&slot| a.ref_frame_map.get(usize::from(slot)).copied())
            .unwrap_or(NO_PICTURE)
    };
    a.layer_word = narrow8(
        la::temporal_layer_id.put(0, u32::from(h.temporal_id))
            | la::spatial_layer_id.put(0, u32::from(h.spatial_id)),
    );
    // Each reference's coded size, before superres's upscaling, as
    // NVIDIA's parser gives it: the upscaled width decodes superres frames
    // wrongly (aom-720p-superres, 2026-10-24).
    for (reference, &slot) in a.ref_frame.iter_mut().zip(&h.ref_frame_idx) {
        let picture = p.references.get(usize::from(slot)).copied().flatten();
        let (_, height) = picture.map_or((0, 0), Picture::size);
        let width = picture
            .and_then(|picture| usize::try_from(picture.index()).ok())
            .and_then(|index| coded_widths.get(index).copied())
            .unwrap_or(0);
        reference.index = surface(picture);
        reference.width = width;
        reference.height = height;
    }
    for (model, coded) in a.global_motion.iter_mut().zip(&h.global_motion[1..]) {
        model.flags = narrow8(
            gm::invalid.put(0, flag(!coded.shear_is_valid()))
                | gm::wmtype.put(0, u32::from(coded.kind)),
        );
        model.wmmat = coded.params;
    }

    let g = &h.film_grain;
    let mut word = fg::apply_grain.put(0, flag(apply_grain));
    word = fg::overlap_flag.put(word, flag(g.overlap_flag));
    word = fg::scaling_shift_minus8.put(word, u32::from(g.grain_scaling_minus_8));
    word = fg::chroma_scaling_from_luma.put(word, flag(g.chroma_scaling_from_luma));
    word = fg::ar_coeff_lag.put(word, u32::from(g.ar_coeff_lag));
    word = fg::ar_coeff_shift_minus6.put(word, u32::from(g.ar_coeff_shift_minus_6));
    word = fg::grain_scale_shift.put(word, u32::from(g.grain_scale_shift));
    word = fg::clip_to_restricted_range.put(word, flag(g.clip_to_restricted_range));
    a.film_grain_word = narrow16(word);
    a.num_y_points = g.num_y_points;
    a.num_cb_points = g.num_cb_points;
    a.num_cr_points = g.num_cr_points;
    a.random_seed = g.grain_seed;
    a.cb_mult = g.cb_mult;
    a.cb_luma_mult = g.cb_luma_mult;
    a.cb_offset = c_short::try_from(g.cb_offset).unwrap_or(0);
    a.cr_mult = g.cr_mult;
    a.cr_luma_mult = g.cr_luma_mult;
    a.cr_offset = c_short::try_from(g.cr_offset).unwrap_or(0);
    if apply_grain {
        let points = |values: &[u8], scaling: &[u8]| -> Vec<[u8; 2]> {
            values.iter().zip(scaling).map(|(&v, &s)| [v, s]).collect()
        };
        for (slot, point) in a
            .scaling_points_y
            .iter_mut()
            .zip(points(&g.point_y_value, &g.point_y_scaling))
        {
            *slot = point;
        }
        for (slot, point) in a
            .scaling_points_cb
            .iter_mut()
            .zip(points(&g.point_cb_value, &g.point_cb_scaling))
        {
            *slot = point;
        }
        for (slot, point) in a
            .scaling_points_cr
            .iter_mut()
            .zip(points(&g.point_cr_value, &g.point_cr_scaling))
        {
            *slot = point;
        }
        let minus_128 = |coded: u8| c_short::from(coded) - 128;
        a.ar_coeffs_y = g.ar_coeffs_y_plus_128.map(minus_128);
        a.ar_coeffs_cb = g.ar_coeffs_cb_plus_128.map(minus_128);
        a.ar_coeffs_cr = g.ar_coeffs_cr_plus_128.map(minus_128);
    }
    out.CodecSpecific.av1 = a;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_side_of_4n_plus_3_is_one_nvdec_decodes_wrongly() {
        // The sizes measured wrong, and their neighbours measured right.
        for (width, height) in [(643, 363), (643, 360), (640, 363), (427, 240), (1283, 720)] {
            assert!(decodes_wrongly(width, height), "{width}x{height}");
        }
        for (width, height) in [(642, 362), (644, 364), (641, 361), (640, 360), (1920, 1080)] {
            assert!(!decodes_wrongly(width, height), "{width}x{height}");
        }
    }

    #[test]
    fn main_profile_is_taken_at_8_and_10_bits_and_monochrome() {
        assert_eq!(check_format(0, 8, false, true).ok(), Some(8));
        assert_eq!(check_format(0, 10, false, true).ok(), Some(10));
        assert_eq!(check_format(0, 10, true, true).ok(), Some(10));
        assert!(matches!(
            check_format(1, 8, false, false),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            check_format(2, 12, false, true),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            check_format(0, 12, false, true),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            check_format(0, 8, false, false),
            Err(Error::Unsupported(_))
        ));
    }
}
