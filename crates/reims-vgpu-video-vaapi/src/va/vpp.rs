//! Runs of VA-API's video processing (`VAProfileNone`,
//! `VAEntrypointVideoProc`): the JPEG decoder's conversions of its decoded
//! surfaces into the guest's layout (`super::jpeg`). A run takes a region
//! of one VA surface into a window of another, with each side's colour
//! standard, range and chroma siting, and is waited for until a deadline
//! (`super::wait`).
//!
//! A run into a surface of the driver's ([`render`]) is waited for by its
//! target's questions alone, `vaSyncSurface2` with a timeout of 0, each of
//! which takes the display's lock only as it asks. **Measured** on the RX
//! 7900 XTX and the Radeon 780M (radeonsi, Mesa 26.2.4, 2026-10-26): a 4K
//! H.264 decode's calls took 0.046 to 0.050 ms at the median and 0.09 to
//! 0.12 ms at the 99th percentile alone, and 0.055 to 0.064 ms and 0.14 to
//! 0.31 ms beside a thread decoding 4K JPEGs at 1/2 into NV12 and into
//! BGRA, each a run asked after every 100 µs; so no `poll` of a dma-buf
//! goes first, as for the decoders' copy back (`super::fence`). A run into
//! host memory of ours, a conversion's last ([`submit`]), is waited for
//! as the copy back's is, on the memory's dma-buf first
//! (`super::readback::HostSurface`).

use reims_vgpu_video_core::runtime::MonoTime;

use super::buffer::{Buffer, render_picture};
use super::caps::Context;
use super::display::{Display, Error};
use super::ffi;
use super::vpp_ffi as vpp;
use super::wait::{self, Work};

/// The size a processing context is made at. Mesa's video processing takes
/// any size per run; a context's size is the decoders' concern.
pub(super) const CONTEXT_SIZE: (u32, u32) = (1920, 1080);

/// What Mesa rounds a video surface's sides up to (`VL_MACROBLOCK_WIDTH`
/// and `_HEIGHT`, as measured: a turned 160x120 surface was read as
/// 160x128, a 160x240 one as itself).
pub(super) const BUFFER_ALIGNMENT: u32 = 16;

/// One run of the pipeline.
#[derive(Clone, Copy)]
pub(super) struct Pass {
    /// The source surface.
    pub(super) surface: ffi::VASurfaceID,
    /// The region of it; None for all.
    pub(super) region: Option<vpp::VARectangle>,
    /// The target surface.
    pub(super) target: ffi::VASurfaceID,
    /// The window of it; None for all.
    pub(super) window: Option<vpp::VARectangle>,
    /// The colour standard and range of the source.
    pub(super) from: (vpp::VAProcColorStandardType, u8),
    /// Of the target.
    pub(super) to: (vpp::VAProcColorStandardType, u8),
    /// `filter_flags`.
    pub(super) filter: u32,
    /// `rotation_state` and `mirror_state`.
    pub(super) turn: (u32, u32),
    /// The source's `chroma_sample_location`, `VA_CHROMA_SITING_*`, or 0,
    /// unknown, for an RGB one.
    pub(super) siting: u8,
    /// The target's, or 0 for an RGB one or one left to the driver.
    pub(super) out_siting: u8,
}

/// Runs one pass of the pipeline, and waits for its target until
/// `deadline` (`super::wait`): past it, [`Error::Stalled`], the run may
/// still write the target.
pub(super) fn render(
    display: &Display,
    context: &Context,
    pass: &Pass,
    deadline: MonoTime,
) -> Result<(), Error> {
    submit(display, context, pass)?;
    // The target is a surface of this display, alive for the wait: `pass`'s
    // owner holds it.
    wait::wait_for(
        display,
        Work::Surface(pass.target),
        deadline,
        "a video-processing run in time",
    )
}

/// Submits one pass of the pipeline, not waited for: its owner waits for
/// its target.
pub(super) fn submit(display: &Display, context: &Context, pass: &Pass) -> Result<(), Error> {
    let mut params = vpp::VAProcPipelineParameterBuffer::zeroed();
    params.surface = pass.surface;
    if let Some(region) = &pass.region {
        params.surface_region = region;
    }
    if let Some(window) = &pass.window {
        params.output_region = window;
    }
    params.surface_color_standard = pass.from.0;
    params.input_color_properties.color_range = pass.from.1;
    params.input_color_properties.chroma_sample_location = pass.siting;
    params.output_color_properties.chroma_sample_location = pass.out_siting;
    params.output_color_standard = pass.to.0;
    params.output_color_properties.color_range = pass.to.1;
    // Alpha 0: the driver renders straight into the target (va_vpp.h,
    // output_background_color); what is outside the window is not read
    // back.
    params.output_background_color = 0;
    params.filter_flags = pass.filter;
    (params.rotation_state, params.mirror_state) = pass.turn;
    let buffer = Buffer::with_value(
        display,
        context.id,
        vpp::VAProcPipelineParameterBufferType,
        &params,
    )?;
    // The pipeline's two rectangles are `pass`'s, borrowed past the render.
    render_picture(display, context.id, pass.target, &[&[&buffer]])
}
