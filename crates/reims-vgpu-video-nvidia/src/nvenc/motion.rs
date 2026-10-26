//! NVENC's motion estimation alone: an H.264 session in ME-only mode
//! (`NV_ENC_INITIALIZE_PARAMS::enableMEOnlyMode`), two NV12 input buffers
//! and one motion vector buffer, and `nvEncRunMotionEstimationOnly` on a
//! pair of frames, whose vectors come back one macroblock at a time
//! (`NV_ENC_H264_MV_DATA`, in quarter pixels).
//!
//! H.264's, not HEVC's: its macroblocks are a fixed grid of 16x16 with a
//! vector for each 8x8 block, where HEVC's CUs vary in size across a CTB.

use std::sync::Arc;

use super::encoder::preset_guid;
use super::ffi::{self, Zeroed};
use super::motion_ffi::{NV_ENC_CAPS_SUPPORT_MEONLY_MODE, NV_ENC_H264_MV_DATA, enableMEOnlyMode};
use super::{Buffer, Session};
use crate::cuda::Gpu;
use crate::error::Error;

/// What H.264's ME-only mode takes on a GPU, as NVENC answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MotionLimits {
    pub(crate) min_width: u32,
    pub(crate) min_height: u32,
    pub(crate) max_width: u32,
    pub(crate) max_height: u32,
}

/// Asks NVENC whether its H.264 encoder runs motion estimation alone on
/// `gpu`, and the sizes it takes; `Unsupported` where it does not.
pub(crate) fn motion_limits(gpu: Arc<Gpu>) -> Result<MotionLimits, Error> {
    let session = Session::open(gpu)?;
    let codec = ffi::NV_ENC_CODEC_H264_GUID;
    if !session.codec_guids()?.contains(&codec) {
        return Err(Error::Unsupported("NVENC encodes no H.264".into()));
    }
    if session.capability(codec, NV_ENC_CAPS_SUPPORT_MEONLY_MODE)? != 1 {
        return Err(Error::Unsupported(
            "NVENC's H.264 encoder has no ME-only mode".into(),
        ));
    }
    if !session
        .input_formats(codec)?
        .contains(&ffi::NV_ENC_BUFFER_FORMAT_NV12)
    {
        return Err(Error::Unsupported(
            "NVENC's H.264 encoder takes no NV12".into(),
        ));
    }
    let cap = |capability| {
        session
            .capability(codec, capability)
            .map(|value| u32::try_from(value).unwrap_or(0))
    };
    Ok(MotionLimits {
        min_width: cap(ffi::caps::WIDTH_MIN)?,
        min_height: cap(ffi::caps::HEIGHT_MIN)?,
        max_width: cap(ffi::caps::WIDTH_MAX)?,
        max_height: cap(ffi::caps::HEIGHT_MAX)?,
    })
}

/// One NV12 frame for [`NvencMotion::estimate`]: each plane's bytes and
/// the bytes from one row to the next.
pub(crate) struct Nv12<'a> {
    pub(crate) luma: (&'a [u8], usize),
    pub(crate) chroma: (&'a [u8], usize),
}

/// An H.264 session in ME-only mode, with its buffers.
pub(crate) struct NvencMotion {
    width: u32,
    height: u32,
    reference: Buffer,
    current: Buffer,
    vectors: Buffer,
    session: Session,
}

impl NvencMotion {
    /// A session for `width` by `height` NV12 frames, even sizes within
    /// [`motion_limits`]'s, at preset P`preset` (1 to 7: the higher, the
    /// wider NVENC searches).
    pub(crate) fn new(gpu: Arc<Gpu>, width: u32, height: u32, preset: u8) -> Result<Self, Error> {
        let mut session = Session::open(gpu)?;
        let codec = ffi::NV_ENC_CODEC_H264_GUID;
        let preset = preset_guid(preset)?;
        let tuning = ffi::NV_ENC_TUNING_INFO_HIGH_QUALITY;
        let mut config = session.preset_config(codec, preset, tuning)?.presetCfg;
        // One reference, the one each call names, and no B pictures.
        config.gopLength = ffi::NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1;
        let mut init = ffi::NV_ENC_INITIALIZE_PARAMS::zeroed();
        init.version = ffi::NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = codec;
        init.presetGUID = preset;
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        init.frameRateNum = 30;
        init.frameRateDen = 1;
        init.enableEncodeAsync = 0;
        init.enablePTD = 1;
        init.flags = enableMEOnlyMode.put(init.flags, 1);
        init.maxEncodeWidth = width;
        init.maxEncodeHeight = height;
        // `tuningInfo` stays undefined: with it set, the driver refused the
        // session, "tuningInfo is not supported with H264 Meonly mode" (RTX
        // 5080 Laptop GPU, driver 615.71.09, 2026-10-24). The preset's
        // configuration is still the tuning's.
        session.initialize(init, config)?;
        let format = ffi::NV_ENC_BUFFER_FORMAT_NV12;
        Ok(Self {
            width,
            height,
            reference: session.create_input(width, height, format)?,
            current: session.create_input(width, height, format)?,
            vectors: session.create_vectors()?,
            session,
        })
    }

    /// The vectors of `current` against `reference`, both the session's
    /// size: one entry a macroblock, in raster order.
    pub(crate) fn estimate(
        &mut self,
        reference: &Nv12<'_>,
        current: &Nv12<'_>,
    ) -> Result<Vec<NV_ENC_H264_MV_DATA>, Error> {
        self.upload(self.reference, reference)?;
        self.upload(self.current, current)?;
        self.session
            .estimate_motion(self.current, self.reference, self.vectors)?;
        let blocks = usize::try_from(self.width.div_ceil(16))
            .unwrap_or(0)
            .saturating_mul(usize::try_from(self.height.div_ceil(16)).unwrap_or(0));
        self.session.lock_bitstream(self.vectors, |locked| {
            let records = locked.bitstreamBufferPtr.cast::<NV_ENC_H264_MV_DATA>();
            if records.is_null() || !records.is_aligned() {
                return Err(Error::Unsupported(
                    "NVENC's vector buffer is not at an address of its records".into(),
                ));
            }
            // SAFETY: while locked, the vector buffer holds one
            // NV_ENC_H264_MV_DATA a macroblock of the session's frames,
            // written by the run (`NvEncCreateMVBuffer`: its size follows
            // the input buffers'), at an aligned address (checked).
            Ok(unsafe { std::slice::from_raw_parts(records, blocks) }.to_vec())
        })
    }

    /// Copies `frame` into `input`, the luma's rows then the chroma's at
    /// NVENC's pitch.
    fn upload(&self, input: Buffer, frame: &Nv12<'_>) -> Result<(), Error> {
        let width = usize::try_from(self.width).unwrap_or(0);
        let height = usize::try_from(self.height).unwrap_or(0);
        self.session
            .upload(input, frame.luma, frame.chroma, width, height)
            .map(|_| ())
    }
}
