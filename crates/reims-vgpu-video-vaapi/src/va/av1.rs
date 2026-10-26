//! AV1 decoding on VA-API: [`crate::av1::Decoder`] over an accelerator
//! that fills libva's AV1 buffers (`va_dec_av1.h`) and decodes into a pool
//! of NV12 or P010 surfaces, with a second pool for frames whose film
//! grain the driver applies.
//!
//! The conventions it follows, as libva's users and Mesa's frontend
//! (`src/gallium/frontends/va/picture_av1.c`) take them:
//! - `frame_width_minus1` is the upscaled width; the driver works out the
//!   coded width from `superres_scale_denominator`;
//! - every value that a frame inherits (segmentation features, loop
//!   filter deltas, global motion, film grain) is passed as in force, not
//!   as coded;
//! - each tile group is one slice parameter buffer with an element per
//!   tile, offsets relative to the slice data buffer that follows it,
//!   which holds the group's tile data;
//! - with film grain, `current_frame` (the render target) is the frame
//!   as later frames predict from it, and `current_display_picture` takes
//!   it with the grain applied.

mod ffi;

use super::buffer::{Buffer, render_decode};
use super::caps::{Config, Context, Entrypoint, Profile};
use super::display::{Display, Error};
use super::ffi as va;
use super::frame::Frame;
use super::surface::{Rect, Surface, SurfaceFormat, SurfacePool, end_context};
use super::wait::{self, Deadline};
use crate::av1::{
    Accelerator, Decoder, FrameParams, Output, Step,
    av1c::ConfigurationRecord,
    frame::{FrameHeader, FrameType, NUM_REF_FRAMES},
    sequence::SequenceHeader,
    tile_group::{Tile, TileGroup},
};

/// `VAProfileAV1Profile0`, Main: 8 and 10 bits at 4:2:0, the profile
/// this decoder runs.
pub const AV1_MAIN: Profile = Profile(ffi::VAProfileAV1Profile0);

/// Surfaces a pool makes beyond the eight reference slots and the
/// current frame, so that a frame the caller still holds does not stall
/// the next decode.
const SPARE_SURFACES: usize = 2;

/// An AV1 decoder on a VA display.
///
/// Samples are temporal units in the low overhead bitstream format, as
/// MP4 samples and IVF frames are. Each frame a sample shows comes out,
/// in the order the stream shows them.
pub struct Av1Decoder {
    inner: Decoder<VaAv1>,
}

impl Av1Decoder {
    /// A decoder for samples described by an `av1C` record (AV1 Codec ISO
    /// Media File Format Binding, 2.3). Its sequence header, or failing
    /// one its own fields, is checked against what the decoder and the
    /// driver take, so that a stream they cannot decode is refused here.
    pub fn new(display: &Display, av1c: &[u8]) -> Result<Self, Error> {
        let inner = Decoder::from_av1c(VaAv1::new(display), av1c)?;
        if let Some(sequence) = inner.sequence() {
            select(display, sequence)?;
        } else {
            let record = ConfigurationRecord::parse(av1c)?;
            check_format(
                record.seq_profile,
                record.bit_depth(),
                record.monochrome,
                record.chroma_subsampling_x && record.chroma_subsampling_y,
            )?;
        }
        Ok(Self { inner })
    }

    /// A decoder whose samples carry their own sequence headers, as an
    /// IVF file's do.
    pub fn new_obu_stream(display: &Display) -> Self {
        Self {
            inner: Decoder::new(VaAv1::new(display)),
        }
    }

    /// Decodes one temporal unit, tagged `id`, and returns the frames it
    /// shows. The decodes may still be running on the GPU: a frame's
    /// readback waits for it.
    pub fn decode(&mut self, sample: &[u8], id: u64) -> Result<Vec<Frame>, Error> {
        // One deadline for the call's driver calls and its frames' read
        // backs (`super::wait`).
        let deadline = Deadline::default();
        let outputs = wait::by(&deadline, || self.inner.decode(sample, id))?;
        Ok(outputs
            .into_iter()
            .map(|output| frame_from_output(output, &deadline))
            .collect())
    }

    /// Every frame the last [`Av1Decoder::decode`] decoded or showed
    /// again, in order, each spatial layer's included.
    pub fn steps(&self) -> &[Step] {
        self.inner.steps()
    }

    /// Forgets every reference, as for a seek: decoding starts again at
    /// the next key frame.
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// The sequence header in force, once one has come.
    pub fn sequence(&self) -> Option<&SequenceHeader> {
        self.inner.sequence()
    }

    /// The VA profile the decoder runs, once it has started a frame.
    pub fn profile(&self) -> Option<Profile> {
        self.inner.accelerator().session.as_ref().map(|s| s.profile)
    }
}

const _: fn() = || {
    fn send<T: Send>() {}
    send::<Av1Decoder>();
};

/// The frame VA-API decoded for one of the decoder's outputs.
fn frame_from_output(output: Output<Surface>, deadline: &Deadline) -> Frame {
    Frame {
        surface: output.picture,
        id: output.id,
        poc: i32::try_from(output.order_hint).unwrap_or(0),
        idr: output.frame_type == FrameType::Key,
        reference: false,
        rect: Rect {
            x: 0,
            y: 0,
            width: output.width,
            height: output.height,
        },
        coded_size: (output.width, output.height),
        full_range: output.full_range,
        deadline: deadline.clone(),
    }
}

/// What the decoder refuses of a stream's format, whatever the driver:
/// all but Main (`seq_profile` 0), so 4:4:4 and 4:2:2, and 12 bits.
/// Monochrome Main streams are taken: the driver gets `mono_chrome`.
fn check_format(
    seq_profile: u8,
    bit_depth: u8,
    mono_chrome: bool,
    subsampled_420: bool,
) -> Result<SurfaceFormat, Error> {
    match seq_profile {
        0 => {}
        1 => return Err(Error::Unsupported("AV1 High profile (4:4:4)".into())),
        _ => return Err(Error::Unsupported("AV1 Professional profile".into())),
    }
    if !mono_chrome && !subsampled_420 {
        return Err(Error::Unsupported("AV1 chroma other than 4:2:0".into()));
    }
    match bit_depth {
        8 => Ok(SurfaceFormat::Nv12),
        10 => Ok(SurfaceFormat::P010),
        other => Err(Error::Unsupported(format!("AV1 at {other} bits"))),
    }
}

/// What `sequence`'s frames decode with on this driver: the profile, the
/// surface format and the largest picture the driver takes, or why it
/// cannot decode them.
fn select(
    display: &Display,
    sequence: &SequenceHeader,
) -> Result<(Profile, SurfaceFormat, (u32, u32)), Error> {
    let c = &sequence.color;
    let format = check_format(
        sequence.seq_profile,
        c.bit_depth,
        c.mono_chrome,
        c.subsampling_x && c.subsampling_y,
    )?;
    if !display.supports(AV1_MAIN, Entrypoint::VLD) {
        return Err(Error::Unsupported(
            "AV1 Main: the driver has no VAProfileAV1Profile0".into(),
        ));
    }
    let limits = display.config_attributes(
        AV1_MAIN,
        Entrypoint::VLD,
        &[
            va::VAConfigAttribRTFormat,
            va::VAConfigAttribMaxPictureWidth,
            va::VAConfigAttribMaxPictureHeight,
        ],
    )?;
    if limits[0].is_some_and(|formats| formats & format.rt_format() == 0) {
        return Err(Error::Unsupported(format!(
            "AV1 at {} bits: the driver's AV1 Main has no {}",
            c.bit_depth,
            format.name()
        )));
    }
    let max = (limits[1].unwrap_or(u32::MAX), limits[2].unwrap_or(u32::MAX));
    Ok((AV1_MAIN, format, max))
}

/// The size of the surfaces `sequence`'s frames decode into: the largest
/// frame it allows, when the driver takes that. An encoder may allow far
/// more than it codes (SVT-AV1 with switch frames allows 16384x8704 for a
/// 720p stream, measured 2026-10-24), so above the driver's maximum the
/// first frame's size is taken instead, and a later frame larger than it
/// is refused when it comes.
fn surface_size(
    sequence: &SequenceHeader,
    first: &FrameHeader,
    driver_max: (u32, u32),
) -> Result<(u32, u32), Error> {
    let fits = |(width, height): (u32, u32)| width <= driver_max.0 && height <= driver_max.1;
    let largest = sequence.max_frame_size();
    if fits(largest) {
        return Ok(largest);
    }
    let frame = (first.upscaled_width, first.frame_height);
    if fits(frame) {
        return Ok(frame);
    }
    Err(Error::Unsupported(format!(
        "AV1 at {}x{}: the driver's maximum is {}x{}",
        frame.0, frame.1, driver_max.0, driver_max.1
    )))
}

/// The VA-API half of the decoder.
struct VaAv1 {
    display: Display,
    session: Option<Session>,
}

/// What one sequence's frames are decoded with.
struct Session {
    profile: Profile,
    /// The surfaces' size: no frame larger than this decodes.
    size: (u32, u32),
    /// Destroyed first, before the surfaces it renders into: it holds
    /// its config.
    context: Option<Context>,
    pool: SurfacePool,
    /// Frames with their film grain, when the sequence has film grain.
    display_pool: Option<SurfacePool>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The context goes first. The pools may outlive it: frames the
        // caller holds keep their surfaces.
        end_context(
            self.context.take(),
            std::iter::once(&self.pool).chain(&self.display_pool),
        );
    }
}

impl VaAv1 {
    fn new(display: &Display) -> Self {
        Self {
            display: display.clone_ref(),
            session: None,
        }
    }

    fn session(&self) -> Result<&Session, Error> {
        self.session
            .as_ref()
            .ok_or_else(|| Error::Unsupported("decoding before a sequence header".into()))
    }
}

impl Accelerator for VaAv1 {
    type Picture = Surface;
    type Error = Error;

    fn configure(&mut self, sequence: &SequenceHeader, first: &FrameHeader) -> Result<(), Error> {
        self.session = None;
        let (profile, format, driver_max) = select(&self.display, sequence)?;
        let size = surface_size(sequence, first, driver_max)?;
        let (width, height) = size;
        // Even sizes, so that an odd frame's last chroma row and pair are
        // in the surface and its image.
        let (width, height) = (width.next_multiple_of(2), height.next_multiple_of(2));
        let mut attribs = [va::VAConfigAttrib {
            type_: va::VAConfigAttribRTFormat,
            value: format.rt_format(),
        }];
        let config = Config::new(&self.display, profile, Entrypoint::VLD, &mut attribs)?;
        let count = NUM_REF_FRAMES + 1 + SPARE_SURFACES;
        let pool = SurfacePool::new(&self.display, format, width, height, count)?;
        let display_pool = if sequence.film_grain_params_present {
            Some(SurfacePool::new(
                &self.display,
                format,
                width,
                height,
                count,
            )?)
        } else {
            None
        };
        let mut targets = pool.ids();
        if let Some(display_pool) = &display_pool {
            targets.extend(display_pool.ids());
        }
        let context = Context::new(&self.display, config, width, height, &mut targets)?;
        self.session = Some(Session {
            profile,
            size,
            context: Some(context),
            pool,
            display_pool,
        });
        Ok(())
    }

    fn new_picture(&mut self) -> Result<Surface, Error> {
        self.session()?.pool.get()
    }

    fn new_display_picture(&mut self) -> Result<Surface, Error> {
        self.session()?
            .display_pool
            .as_ref()
            .ok_or_else(|| Error::Unsupported("film grain in a sequence without it".into()))?
            .get()
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, Surface>) -> Result<(), Error> {
        let session = self.session()?;
        let (width, height) = (p.header.upscaled_width, p.header.frame_height);
        if width > session.size.0 || height > session.size.1 {
            return Err(Error::Unsupported(format!(
                "an AV1 frame of {width}x{height}, larger than the first frame of a \
                 sequence that allows more than the driver takes ({}x{})",
                session.size.0, session.size.1
            )));
        }
        let decoding = session
            .context
            .as_ref()
            .ok_or_else(|| Error::Unsupported("decoding without a context".into()))?;
        let context = decoding.id;
        let display = &self.display;
        let picture_buffer = Buffer::with_value(
            display,
            context,
            va::VAPictureParameterBufferType,
            &picture_parameters(p),
        )?;
        let mut group_buffers = Vec::with_capacity(p.tile_groups.len());
        for group in p.tile_groups {
            let tiles: Vec<ffi::VASliceParameterBufferAV1> = group
                .tiles
                .iter()
                .map(|tile| tile_parameters(group, tile))
                .collect::<Result<_, _>>()?;
            group_buffers.push((
                Buffer::with_values(display, context, va::VASliceParameterBufferType, &tiles)?,
                Buffer::with_bytes(display, context, va::VASliceDataBufferType, group.data)?,
            ));
        }
        // One render of them all, each tile group's parameters ahead of its
        // data (module docs).
        let mut all = vec![&picture_buffer];
        for (parameters, data) in &group_buffers {
            all.extend([parameters, data]);
        }
        // A first picture past its deadline may still be submitted: the
        // pool is kept until the GPU is done with it.
        p.picture
            .stalled_if(render_decode(display, decoding, p.picture.id(), &[&all]))
    }
}

/// One tile's slice parameters: its data within its group's buffer.
fn tile_parameters(
    group: &TileGroup<'_>,
    tile: &Tile,
) -> Result<ffi::VASliceParameterBufferAV1, Error> {
    let narrow = |v: u32| u16::try_from(v).map_err(|_| Error::Unsupported("too many tiles".into()));
    let bytes =
        |v: usize| u32::try_from(v).map_err(|_| Error::Unsupported("a tile over 4 GiB".into()));
    Ok(ffi::VASliceParameterBufferAV1 {
        slice_data_size: bytes(tile.size)?,
        slice_data_offset: bytes(tile.offset)?,
        slice_data_flag: va::VA_SLICE_DATA_FLAG_ALL,
        tile_row: narrow(tile.row)?,
        tile_column: narrow(tile.column)?,
        tg_start: narrow(group.tg_start)?,
        tg_end: narrow(group.tg_end)?,
        ..ffi::VASliceParameterBufferAV1::default()
    })
}

/// `VADecPictureParameterBufferAV1` for one frame.
fn picture_parameters(p: &FrameParams<'_, Surface>) -> ffi::VADecPictureParameterBufferAV1 {
    use ffi::{
        film_grain_info_fields as fg, loop_filter_info_fields as lf, loop_restoration_fields as lr,
        mode_control_fields as mc, pic_info_fields as pic, qmatrix_fields as qm,
        segment_info_fields as seg, seq_info_fields as sq,
    };
    let (s, h) = (p.sequence, p.header);
    let c = &s.color;
    let flag = u32::from;
    let seq_info_fields = sq::STILL_PICTURE.put(flag(s.still_picture))
        | sq::USE_128X128_SUPERBLOCK.put(flag(s.use_128x128_superblock))
        | sq::ENABLE_FILTER_INTRA.put(flag(s.enable_filter_intra))
        | sq::ENABLE_INTRA_EDGE_FILTER.put(flag(s.enable_intra_edge_filter))
        | sq::ENABLE_INTERINTRA_COMPOUND.put(flag(s.enable_interintra_compound))
        | sq::ENABLE_MASKED_COMPOUND.put(flag(s.enable_masked_compound))
        | sq::ENABLE_DUAL_FILTER.put(flag(s.enable_dual_filter))
        | sq::ENABLE_ORDER_HINT.put(flag(s.enable_order_hint))
        | sq::ENABLE_JNT_COMP.put(flag(s.enable_jnt_comp))
        | sq::ENABLE_CDEF.put(flag(s.enable_cdef))
        | sq::MONO_CHROME.put(flag(c.mono_chrome))
        | sq::COLOR_RANGE.put(flag(c.color_range))
        | sq::SUBSAMPLING_X.put(flag(c.subsampling_x))
        | sq::SUBSAMPLING_Y.put(flag(c.subsampling_y))
        | sq::CHROMA_SAMPLE_POSITION.put(u32::from(c.chroma_sample_position))
        | sq::FILM_GRAIN_PARAMS_PRESENT.put(flag(s.film_grain_params_present));
    let segmentation = &h.segmentation;
    let g = &h.film_grain;
    let film_grain_info = ffi::VAFilmGrainStructAV1 {
        film_grain_info_fields: fg::APPLY_GRAIN.put(flag(g.apply_grain))
            | fg::CHROMA_SCALING_FROM_LUMA.put(flag(g.chroma_scaling_from_luma))
            | fg::GRAIN_SCALING_MINUS_8.put(u32::from(g.grain_scaling_minus_8))
            | fg::AR_COEFF_LAG.put(u32::from(g.ar_coeff_lag))
            | fg::AR_COEFF_SHIFT_MINUS_6.put(u32::from(g.ar_coeff_shift_minus_6))
            | fg::GRAIN_SCALE_SHIFT.put(u32::from(g.grain_scale_shift))
            | fg::OVERLAP_FLAG.put(flag(g.overlap_flag))
            | fg::CLIP_TO_RESTRICTED_RANGE.put(flag(g.clip_to_restricted_range)),
        grain_seed: g.grain_seed,
        num_y_points: g.num_y_points,
        point_y_value: g.point_y_value,
        point_y_scaling: g.point_y_scaling,
        num_cb_points: g.num_cb_points,
        point_cb_value: g.point_cb_value,
        point_cb_scaling: g.point_cb_scaling,
        num_cr_points: g.num_cr_points,
        point_cr_value: g.point_cr_value,
        point_cr_scaling: g.point_cr_scaling,
        ar_coeffs_y: g.ar_coeffs_y_plus_128.map(minus_128),
        ar_coeffs_cb: g.ar_coeffs_cb_plus_128.map(minus_128),
        ar_coeffs_cr: g.ar_coeffs_cr_plus_128.map(minus_128),
        cb_mult: g.cb_mult,
        cb_luma_mult: g.cb_luma_mult,
        cb_offset: g.cb_offset,
        cr_mult: g.cr_mult,
        cr_luma_mult: g.cr_luma_mult,
        cr_offset: g.cr_offset,
        va_reserved: [0; 4],
    };
    let t = &h.tile_info;
    let mut width_in_sbs_minus_1 = [0u16; 63];
    for (slot, &width) in width_in_sbs_minus_1.iter_mut().zip(&t.width_in_sbs) {
        *slot = u16::try_from(width.saturating_sub(1)).unwrap_or(u16::MAX);
    }
    let mut height_in_sbs_minus_1 = [0u16; 63];
    for (slot, &height) in height_in_sbs_minus_1.iter_mut().zip(&t.height_in_sbs) {
        *slot = u16::try_from(height.saturating_sub(1)).unwrap_or(u16::MAX);
    }
    let pic_info_fields = pic::FRAME_TYPE.put(h.frame_type as u32)
        | pic::SHOW_FRAME.put(flag(h.show_frame))
        | pic::SHOWABLE_FRAME.put(flag(h.showable_frame))
        | pic::ERROR_RESILIENT_MODE.put(flag(h.error_resilient_mode))
        | pic::DISABLE_CDF_UPDATE.put(flag(h.disable_cdf_update))
        | pic::ALLOW_SCREEN_CONTENT_TOOLS.put(flag(h.allow_screen_content_tools))
        | pic::FORCE_INTEGER_MV.put(flag(h.force_integer_mv))
        | pic::ALLOW_INTRABC.put(flag(h.allow_intrabc))
        | pic::USE_SUPERRES.put(flag(h.use_superres))
        | pic::ALLOW_HIGH_PRECISION_MV.put(flag(h.allow_high_precision_mv))
        | pic::IS_MOTION_MODE_SWITCHABLE.put(flag(h.is_motion_mode_switchable))
        | pic::USE_REF_FRAME_MVS.put(flag(h.use_ref_frame_mvs))
        | pic::DISABLE_FRAME_END_UPDATE_CDF.put(flag(h.disable_frame_end_update_cdf))
        | pic::UNIFORM_TILE_SPACING_FLAG.put(flag(t.uniform))
        | pic::ALLOW_WARPED_MOTION.put(flag(h.allow_warped_motion))
        // Large scale tile decoding is refused before this.
        | pic::LARGE_SCALE_TILE.put(0);
    let l = &h.loop_filter;
    let q = &h.quantization;
    let narrow8 = |v: u32| u8::try_from(v).unwrap_or(u8::MAX);
    let narrow16 = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    let cdef = &h.cdef;
    let strengths = |pri: &[u8; 8], sec: &[u8; 8]| -> [u8; 8] {
        core::array::from_fn(|i| (pri[i] << 2) | (sec[i] & 3))
    };
    let restoration = &h.loop_restoration;
    // Shown key frames refer to nothing, and libva's users say so.
    let shown_key = h.frame_type == FrameType::Key && h.show_frame;
    let ref_frame_map = core::array::from_fn(|i| match p.references[i] {
        Some(reference) if !shown_key => reference.id(),
        _ => va::VA_INVALID_SURFACE,
    });
    ffi::VADecPictureParameterBufferAV1 {
        profile: s.seq_profile,
        order_hint_bits_minus_1: s.order_hint_bits.saturating_sub(1),
        bit_depth_idx: match c.bit_depth {
            8 => 0,
            10 => 1,
            _ => 2,
        },
        matrix_coefficients: c.matrix_coefficients,
        seq_info_fields,
        current_frame: p.picture.id(),
        current_display_picture: p.display.map_or(p.picture.id(), Surface::id),
        anchor_frames_num: 0,
        anchor_frames_list: core::ptr::null_mut(),
        frame_width_minus1: narrow16(h.upscaled_width - 1),
        frame_height_minus1: narrow16(h.frame_height - 1),
        output_frame_width_in_tiles_minus_1: 0,
        output_frame_height_in_tiles_minus_1: 0,
        ref_frame_map,
        ref_frame_idx: h.ref_frame_idx,
        primary_ref_frame: h.primary_ref_frame,
        order_hint: narrow8(h.order_hint),
        seg_info: ffi::VASegmentationStructAV1 {
            segment_info_fields: seg::ENABLED.put(flag(segmentation.enabled))
                | seg::UPDATE_MAP.put(flag(segmentation.update_map))
                | seg::TEMPORAL_UPDATE.put(flag(segmentation.temporal_update))
                | seg::UPDATE_DATA.put(flag(segmentation.update_data)),
            feature_data: segmentation.feature_data,
            feature_mask: segmentation.feature_mask,
            va_reserved: [0; 4],
        },
        film_grain_info,
        tile_cols: narrow8(t.cols),
        tile_rows: narrow8(t.rows),
        width_in_sbs_minus_1,
        height_in_sbs_minus_1,
        tile_count_minus_1: 0,
        context_update_tile_id: narrow16(t.context_update_tile_id),
        pic_info_fields,
        superres_scale_denominator: h.superres_denom,
        interp_filter: h.interpolation_filter,
        filter_level: [l.level[0], l.level[1]],
        filter_level_u: l.level[2],
        filter_level_v: l.level[3],
        loop_filter_info_fields: narrow8(
            lf::SHARPNESS_LEVEL.put(u32::from(l.sharpness))
                | lf::MODE_REF_DELTA_ENABLED.put(flag(l.delta_enabled))
                | lf::MODE_REF_DELTA_UPDATE.put(flag(l.delta_update)),
        ),
        ref_deltas: l.ref_deltas,
        mode_deltas: l.mode_deltas,
        base_qindex: q.base_q_idx,
        y_dc_delta_q: q.delta_q_y_dc,
        u_dc_delta_q: q.delta_q_u_dc,
        u_ac_delta_q: q.delta_q_u_ac,
        v_dc_delta_q: q.delta_q_v_dc,
        v_ac_delta_q: q.delta_q_v_ac,
        qmatrix_fields: narrow16(
            qm::USING_QMATRIX.put(flag(q.using_qmatrix))
                | qm::QM_Y.put(u32::from(q.qm_y))
                | qm::QM_U.put(u32::from(q.qm_u))
                | qm::QM_V.put(u32::from(q.qm_v)),
        ),
        mode_control_fields: mc::DELTA_Q_PRESENT_FLAG.put(flag(h.delta_q_present))
            | mc::LOG2_DELTA_Q_RES.put(u32::from(h.delta_q_res))
            | mc::DELTA_LF_PRESENT_FLAG.put(flag(h.delta_lf_present))
            | mc::LOG2_DELTA_LF_RES.put(u32::from(h.delta_lf_res))
            | mc::DELTA_LF_MULTI.put(flag(h.delta_lf_multi))
            | mc::TX_MODE.put(u32::from(h.tx_mode))
            | mc::REFERENCE_SELECT.put(flag(h.reference_select))
            | mc::REDUCED_TX_SET_USED.put(flag(h.reduced_tx_set))
            | mc::SKIP_MODE_PRESENT.put(flag(h.skip_mode_present)),
        cdef_damping_minus_3: cdef.damping_minus_3,
        cdef_bits: cdef.bits,
        cdef_y_strengths: strengths(&cdef.y_pri_strength, &cdef.y_sec_strength),
        cdef_uv_strengths: strengths(&cdef.uv_pri_strength, &cdef.uv_sec_strength),
        loop_restoration_fields: narrow16(
            lr::YFRAME_RESTORATION_TYPE.put(u32::from(restoration.frame_restoration_type[0]))
                | lr::CBFRAME_RESTORATION_TYPE
                    .put(u32::from(restoration.frame_restoration_type[1]))
                | lr::CRFRAME_RESTORATION_TYPE
                    .put(u32::from(restoration.frame_restoration_type[2]))
                | lr::LR_UNIT_SHIFT.put(u32::from(restoration.lr_unit_shift))
                | lr::LR_UV_SHIFT.put(u32::from(restoration.lr_uv_shift)),
        ),
        wm: core::array::from_fn(|i| {
            let model = &h.global_motion[i + 1];
            let mut wmmat = [0i32; 8];
            wmmat[..6].copy_from_slice(&model.params);
            ffi::VAWarpedMotionParamsAV1 {
                wmtype: i32::from(model.kind),
                wmmat,
                invalid: u8::from(!model.shear_is_valid()),
                va_reserved: [0; 4],
            }
        }),
        va_reserved: [0; 8],
    }
}

/// A film grain coefficient as coded (`..._plus_128`) as VA-API takes it.
fn minus_128(coded: u8) -> i8 {
    i8::try_from(i16::from(coded) - 128).unwrap_or(0)
}
