//! VP9 decoding on VA-API: [`crate::vp9::Decoder`] over an accelerator
//! that fills libva's VP9 buffers (`va_dec_vp9.h`) and decodes into pools
//! of NV12 or P010 surfaces, a pool for each frame size.
//!
//! The conventions it follows, as `va_dec_vp9.h` states them:
//! - the slice data buffer holds the whole frame from its uncompressed
//!   header ("whole or part of partition 0 (at least compressed header) to
//!   the end of frame"), at offset 0, and `frame_header_length_in_bytes`
//!   counts "from bit stream buffer start to the last byte of uncompressed
//!   frame header": the driver reads the compressed header and the tiles,
//!   and keeps the probability contexts;
//! - the fields with "same syntax and semantic as those in VP9 code" take
//!   libvpx's meaning: `mcomp_filter_type` in libvpx's numbering (EIGHTTAP
//!   0, EIGHTTAP_SMOOTH 1, EIGHTTAP_SHARP 2, BILINEAR 3, SWITCHABLE 4), which
//!   swaps the specification's first two, and `frame_context_idx` as
//!   decoding uses it, 0 for an intra or error resilient frame;
//! - `segment_pred_probs` are 255 each without a temporal update, as
//!   `setup_segmentation()` sets them;
//! - every `seg_param` is filled with segmentation on, the first alone
//!   without ("all other entries should be populated with 0");
//! - an empty slot's `reference_frames` entry is `VA_INVALID_SURFACE`.
//!
//! A key frame of a new size or format gets a new config and context. An
//! inter frame of another size keeps them, so that the driver keeps its
//! probability contexts and the last frame's motion vectors, and is
//! decoded into a surface of its own size; its references stay at theirs.
//! Every frame's size is checked against the driver's maximum, since an
//! intra-only or inter frame changes it without a new config.

mod ffi;

use super::buffer::{Buffer, render_decode};
use super::caps::{Config, Context, Entrypoint, Profile};
use super::display::{Display, Error};
use super::ffi as va;
use super::frame::Frame;
use super::surface::{Rect, Surface, SurfaceFormat, SurfacePool, end_context};
use super::wait::{self, Deadline};
use crate::vp9::{
    Accelerator, Decoder, FrameParams, Output, Step, StreamFormat,
    header::{FrameType, InterpolationFilter, NUM_REF_FRAMES},
    vpcc::ConfigurationRecord,
};

/// `VAProfileVP9Profile0`: 8 bits, 4:2:0.
pub const VP9_PROFILE0: Profile = Profile(ffi::VAProfileVP9Profile0);
/// `VAProfileVP9Profile2`: 10 bits here (and 12, which this decoder
/// refuses), 4:2:0.
pub const VP9_PROFILE2: Profile = Profile(ffi::VAProfileVP9Profile2);

/// Surfaces a key frame's pool makes beyond the eight reference slots and
/// the current frame, so that a frame the caller still holds does not stall
/// the next decode. The pool of a size an intra-only or inter frame brings
/// starts at one surface and grows as frames of that size are held, so
/// that a stream alternating sizes makes no more surfaces than it holds.
const SPARE_SURFACES: usize = 2;

/// A VP9 decoder on a VA display.
///
/// Samples are chunks, as MP4 samples and IVF frames are: one frame, or a
/// superframe of several. Each frame a chunk shows comes out, in the order
/// the stream shows them.
pub struct Vp9Decoder {
    inner: Decoder<VaVp9>,
}

impl Vp9Decoder {
    /// A decoder for samples described by a `vpcC` box's contents (VP Codec
    /// ISO Media File Format Binding, 2.2). Its profile, bit depth and
    /// subsampling are checked against what the decoder and the driver
    /// take, so that a stream they cannot decode is refused here.
    pub fn new(display: &Display, vpcc: &[u8]) -> Result<Self, Error> {
        let record = ConfigurationRecord::parse(vpcc)?;
        let (profile, _) = check_format(
            record.profile,
            record.bit_depth,
            record.chroma_subsampling.is_420(),
        )?;
        check_driver(display, profile, record.bit_depth)?;
        Ok(Self::new_stream(display))
    }

    /// A decoder with nothing to check ahead of the first frame, as for an
    /// IVF file.
    pub fn new_stream(display: &Display) -> Self {
        Self {
            inner: Decoder::new(VaVp9 {
                display: display.clone_ref(),
                session: None,
            }),
        }
    }

    /// Decodes one chunk, tagged `id`, and returns the frames it shows.
    /// The decodes may still be running on the GPU: a frame's readback
    /// waits for it.
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

    /// Forgets every reference, as for a seek: decoding starts again at
    /// the next key frame.
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// The frames the last [`Vp9Decoder::decode`] took, those it refused
    /// included, once ([`Decoder::take_steps`]).
    pub fn take_steps(&mut self) -> Vec<Step> {
        self.inner.take_steps()
    }

    /// The VA profile the decoder runs, once it has started a frame.
    pub fn profile(&self) -> Option<Profile> {
        self.inner.accelerator().session.as_ref().map(|s| s.profile)
    }
}

const _: fn() = || {
    fn send<T: Send>() {}
    send::<Vp9Decoder>();
};

/// The frame VA-API decoded for one of the decoder's outputs.
fn frame_from_output(output: Output<Surface>, deadline: &Deadline) -> Frame {
    Frame {
        surface: output.picture,
        id: output.id,
        poc: i32::try_from(output.display_index).unwrap_or(i32::MAX),
        idr: output.frame_type == FrameType::Key,
        reference: output.reference,
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

/// The VA profile and surface format of a stream, or why this decoder
/// refuses it, whatever the driver: Profiles 0 and 2 at 4:2:0, 8 or 10
/// bits.
fn check_format(
    profile: u8,
    bit_depth: u8,
    subsampled_420: bool,
) -> Result<(Profile, SurfaceFormat), Error> {
    let va_profile = match profile {
        0 => VP9_PROFILE0,
        2 => VP9_PROFILE2,
        1 => {
            return Err(Error::Unsupported(
                "VP9 Profile 1 (4:2:2, 4:4:0, 4:4:4)".into(),
            ));
        }
        _ => {
            return Err(Error::Unsupported(
                "VP9 Profile 3 (4:2:2, 4:4:0, 4:4:4 at 10 and 12 bits)".into(),
            ));
        }
    };
    if !subsampled_420 {
        return Err(Error::Unsupported("VP9 chroma other than 4:2:0".into()));
    }
    let format = match (profile, bit_depth) {
        (0, 8) => SurfaceFormat::Nv12,
        (2, 10) => SurfaceFormat::P010,
        (_, other) => {
            return Err(Error::Unsupported(format!(
                "VP9 Profile {profile} at {other} bits"
            )));
        }
    };
    Ok((va_profile, format))
}

/// The largest frame a driver's config decodes, where it says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MaxSize {
    width: Option<u32>,
    height: Option<u32>,
}

impl MaxSize {
    /// Whether a frame of `width` by `height` is within the driver's maximum.
    fn check(self, width: u32, height: u32) -> Result<(), Error> {
        if self.width.is_some_and(|max| width > max) || self.height.is_some_and(|max| height > max)
        {
            return Err(Error::Unsupported(format!(
                "VP9 at {width}x{height}: the driver's maximum is {}x{}",
                self.width.unwrap_or(0),
                self.height.unwrap_or(0)
            )));
        }
        Ok(())
    }
}

/// Whether the driver decodes `profile` with surfaces of `bit_depth`, and
/// the largest frame it decodes so.
fn check_driver(display: &Display, profile: Profile, bit_depth: u8) -> Result<MaxSize, Error> {
    let name = profile.name().unwrap_or("VP9");
    if !display.supports(profile, Entrypoint::VLD) {
        return Err(Error::Unsupported(format!(
            "VP9 at {bit_depth} bits: the driver has no VAProfile{name}"
        )));
    }
    let limits = display.config_attributes(
        profile,
        Entrypoint::VLD,
        &[
            va::VAConfigAttribRTFormat,
            va::VAConfigAttribMaxPictureWidth,
            va::VAConfigAttribMaxPictureHeight,
        ],
    )?;
    let rt_format = if bit_depth > 8 {
        va::VA_RT_FORMAT_YUV420_10
    } else {
        va::VA_RT_FORMAT_YUV420
    };
    if limits[0].is_some_and(|formats| formats & rt_format == 0) {
        return Err(Error::Unsupported(format!(
            "VP9 at {bit_depth} bits: the driver's {name} has no such surfaces"
        )));
    }
    Ok(MaxSize {
        width: limits[1],
        height: limits[2],
    })
}

/// A frame size's surfaces: even, so that an odd frame's last chroma row
/// and pair are in the surface and its image.
fn surface_size(width: u32, height: u32) -> (u32, u32) {
    (width.next_multiple_of(2), height.next_multiple_of(2))
}

/// The VA-API half of the decoder.
struct VaVp9 {
    display: Display,
    session: Option<Session>,
}

/// What frames of one format are decoded with, from a key frame on.
struct Session {
    profile: Profile,
    format: SurfaceFormat,
    /// Destroyed first, before the surfaces it renders into: it holds
    /// its config.
    context: Option<Context>,
    /// Surfaces of the last frame's size, and that size. Surfaces of
    /// other sizes stay alive while a slot or a caller holds them.
    pool: SurfacePool,
    pool_size: (u32, u32),
    max_size: MaxSize,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The context goes first. The pools may outlive it: frames the
        // caller holds keep their surfaces.
        end_context(self.context.take(), [&self.pool]);
    }
}

impl VaVp9 {
    fn session(&self) -> Result<&Session, Error> {
        self.session
            .as_ref()
            .ok_or_else(|| Error::Unsupported("decoding before a key frame".into()))
    }
}

impl Accelerator for VaVp9 {
    type Picture = Surface;
    type Error = Error;

    fn configure(&mut self, format: &StreamFormat, width: u32, height: u32) -> Result<(), Error> {
        self.session = None;
        let (profile, surface_format) = check_format(
            format.profile,
            format.bit_depth,
            format.subsampling_x && format.subsampling_y,
        )?;
        let max_size = check_driver(&self.display, profile, format.bit_depth)?;
        max_size.check(width, height)?;
        let mut attribs = [va::VAConfigAttrib {
            type_: va::VAConfigAttribRTFormat,
            value: surface_format.rt_format(),
        }];
        let config = Config::new(&self.display, profile, Entrypoint::VLD, &mut attribs)?;
        let pool_size = surface_size(width, height);
        let pool = SurfacePool::new(
            &self.display,
            surface_format,
            pool_size.0,
            pool_size.1,
            NUM_REF_FRAMES + 1 + SPARE_SURFACES,
        )?;
        let mut targets = pool.ids();
        let context = Context::new(
            &self.display,
            config,
            pool_size.0,
            pool_size.1,
            &mut targets,
        )?;
        self.session = Some(Session {
            profile,
            format: surface_format,
            context: Some(context),
            pool,
            pool_size,
            max_size,
        });
        Ok(())
    }

    fn new_picture(&mut self, width: u32, height: u32) -> Result<Surface, Error> {
        let display = self.display.clone_ref();
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| Error::Unsupported("decoding before a key frame".into()))?;
        session.max_size.check(width, height)?;
        let size = surface_size(width, height);
        if size != session.pool_size {
            session.pool = SurfacePool::new(&display, session.format, size.0, size.1, 1)?;
            session.pool_size = size;
        }
        session.pool.get()
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, Surface>) -> Result<(), Error> {
        let session = self.session()?;
        let decoding = session
            .context
            .as_ref()
            .ok_or_else(|| Error::Unsupported("decoding without a context".into()))?;
        let context = decoding.id;
        let display = &self.display;
        let picture = Buffer::with_value(
            display,
            context,
            va::VAPictureParameterBufferType,
            &picture_parameters(p)?,
        )?;
        let slice = Buffer::with_value(
            display,
            context,
            va::VASliceParameterBufferType,
            &slice_parameters(p)?,
        )?;
        let data = Buffer::with_bytes(display, context, va::VASliceDataBufferType, p.data)?;
        // A first picture past its deadline may still be submitted: the
        // pool is kept until the GPU is done with it.
        p.picture.stalled_if(render_decode(
            display,
            decoding,
            p.picture.id(),
            &[&[&picture], &[&slice, &data]],
        ))
    }
}

/// `mcomp_filter_type`: libvpx's `INTERP_FILTER` numbering.
fn mcomp_filter_type(filter: InterpolationFilter) -> u32 {
    match filter {
        InterpolationFilter::EightTap => 0,
        InterpolationFilter::EightTapSmooth => 1,
        InterpolationFilter::EightTapSharp => 2,
        InterpolationFilter::Bilinear => 3,
        InterpolationFilter::Switchable => 4,
    }
}

/// `VADecPictureParameterBufferVP9` for one frame.
fn picture_parameters(
    p: &FrameParams<'_, Surface>,
) -> Result<ffi::VADecPictureParameterBufferVP9, Error> {
    use ffi::pic_fields as f;
    let h = p.header;
    let c = &h.color;
    let s = &h.segmentation;
    let flag = u32::from;
    let inter = !h.is_intra();
    let mut pic_fields = f::SUBSAMPLING_X.put(flag(c.subsampling_x))
        | f::SUBSAMPLING_Y.put(flag(c.subsampling_y))
        | f::FRAME_TYPE.put(flag(h.frame_type == FrameType::NonKey))
        | f::SHOW_FRAME.put(flag(h.show_frame))
        | f::ERROR_RESILIENT_MODE.put(flag(h.error_resilient_mode))
        | f::INTRA_ONLY.put(flag(h.intra_only))
        | f::FRAME_PARALLEL_DECODING_MODE.put(flag(h.frame_parallel_decoding_mode))
        | f::RESET_FRAME_CONTEXT.put(u32::from(h.reset_frame_context))
        | f::REFRESH_FRAME_CONTEXT.put(flag(h.refresh_frame_context))
        | f::FRAME_CONTEXT_IDX.put(u32::from(h.frame_context_idx))
        | f::SEGMENTATION_ENABLED.put(flag(s.enabled))
        | f::SEGMENTATION_TEMPORAL_UPDATE.put(flag(s.temporal_update))
        | f::SEGMENTATION_UPDATE_MAP.put(flag(s.update_map))
        | f::LOSSLESS_FLAG.put(flag(h.quantization.lossless()));
    if inter {
        let [last, golden, alt] = h.ref_frame_idx.map(u32::from);
        let [last_bias, golden_bias, alt_bias] = h.ref_frame_sign_bias.map(flag);
        pic_fields |= f::ALLOW_HIGH_PRECISION_MV.put(flag(h.allow_high_precision_mv))
            | f::MCOMP_FILTER_TYPE.put(mcomp_filter_type(h.interpolation_filter))
            | f::LAST_REF_FRAME.put(last)
            | f::LAST_REF_FRAME_SIGN_BIAS.put(last_bias)
            | f::GOLDEN_REF_FRAME.put(golden)
            | f::GOLDEN_REF_FRAME_SIGN_BIAS.put(golden_bias)
            | f::ALT_REF_FRAME.put(alt)
            | f::ALT_REF_FRAME_SIGN_BIAS.put(alt_bias);
    }
    let dimension = |v: u32| {
        u16::try_from(v).map_err(|_| Error::Unsupported(format!("a VP9 frame {v} wide or high")))
    };
    Ok(ffi::VADecPictureParameterBufferVP9 {
        frame_width: dimension(h.width)?,
        frame_height: dimension(h.height)?,
        reference_frames: p
            .references
            .map(|slot| slot.map_or(va::VA_INVALID_SURFACE, Surface::id)),
        pic_fields,
        filter_level: h.loop_filter.level,
        sharpness_level: h.loop_filter.sharpness,
        log2_tile_rows: h.tile_info.rows_log2,
        log2_tile_columns: h.tile_info.cols_log2,
        frame_header_length_in_bytes: u8::try_from(h.uncompressed_header_size)
            .map_err(|_| Error::Unsupported("an uncompressed header over 255 bytes".into()))?,
        first_partition_size: h.header_size_in_bytes,
        mb_segment_tree_probs: s.tree_probs,
        segment_pred_probs: if s.temporal_update {
            s.pred_probs
        } else {
            [255; 3]
        },
        profile: h.profile,
        bit_depth: c.bit_depth,
        va_reserved: [0; 8],
    })
}

/// `VASliceParameterBufferVP9`: the whole frame, and each segment's
/// quantizer steps, filter levels and reference and skip features.
fn slice_parameters(p: &FrameParams<'_, Surface>) -> Result<ffi::VASliceParameterBufferVP9, Error> {
    use ffi::segment_flags as f;
    let seg_param = p.segments.map(|segment| {
        let flags = f::SEGMENT_REFERENCE_ENABLED.put(u32::from(segment.reference_enabled))
            | f::SEGMENT_REFERENCE.put(u32::from(segment.reference))
            | f::SEGMENT_REFERENCE_SKIPPED.put(u32::from(segment.skip));
        ffi::VASegmentParameterVP9 {
            segment_flags: u16::try_from(flags).unwrap_or(0),
            filter_level: segment.filter_levels,
            luma_ac_quant_scale: segment.quantizers.luma_ac,
            luma_dc_quant_scale: segment.quantizers.luma_dc,
            chroma_ac_quant_scale: segment.quantizers.chroma_ac,
            chroma_dc_quant_scale: segment.quantizers.chroma_dc,
            va_reserved: [0; 4],
        }
    });
    Ok(ffi::VASliceParameterBufferVP9 {
        slice_data_size: u32::try_from(p.data.len())
            .map_err(|_| Error::Unsupported("a frame over 4 GiB".into()))?,
        slice_data_offset: 0,
        slice_data_flag: va::VA_SLICE_DATA_FLAG_ALL,
        seg_param,
        va_reserved: [0; 4],
    })
}

#[cfg(test)]
mod tests {
    use super::{Error, MaxSize};

    #[test]
    fn a_frame_past_the_drivers_maximum_is_refused() {
        let max = MaxSize {
            width: Some(4096),
            height: Some(2304),
        };
        assert!(max.check(4096, 2304).is_ok());
        assert!(matches!(max.check(4097, 64), Err(Error::Unsupported(_))));
        assert!(matches!(max.check(64, 2305), Err(Error::Unsupported(_))));
        let unsaid = MaxSize::default();
        assert!(unsaid.check(65536, 65536).is_ok());
    }
}
