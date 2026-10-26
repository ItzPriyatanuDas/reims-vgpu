//! The NVIDIA backend: decode on NVDEC and encode on NVENC, over the CUDA
//! driver API, with nothing above them.
//!
//! The driver's libraries (libcuda, libnvcuvid, libnvidia-encode), and
//! libnvjpeg where there is one, are loaded at run time, so that a host
//! with no NVIDIA driver runs without them: opening the backend there
//! fails, and nothing else does. Their declarations are ours, checked
//! against NVIDIA's headers on the test host (`layout`'s test), but for
//! libnvjpeg's, written from `nvjpeg.h` (`jpeg::nvjpeg`).
//!
//! - [`Gpu`] is a GPU and its CUDA context; [`devices`] lists the host's.
//! - [`decode_capabilities`] and [`encoder_report`] say what NVDEC and
//!   NVENC answer.
//! - [`NvencEncoder`] encodes raw frames to H.264 or HEVC, and
//!   [`NvidiaEncoders`] is core's encoder backend of them.
//! - [`NvidiaMotion`] is core's motion-estimation part, on NVENC's ME-only
//!   mode.
//! - [`NvidiaJpegDecoder`] is core's JPEG decoder, on nvJPEG's hardware
//!   decoder, where libnvjpeg (a CUDA toolkit library, which the driver
//!   does not install) is on the loader's path.
//!
//! Every `unsafe` block holds one operation and says why it is sound, and
//! every `unsafe fn` its contract (the lints below).

#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block,
    clippy::missing_safety_doc
)]

mod backend;
mod cuda;
mod decode;
mod dl;
mod encode;
mod error;
mod jpeg;
#[cfg(test)]
mod layout;
mod motion;
mod nvdec;
mod nvenc;
mod probe;
mod wait;
mod zero_copy;

pub use backend::{NAME, open_backend};
pub use cuda::{DeviceInfo, Gpu, GpuSelector};
pub use decode::NvidiaDecoders;
pub use dl::LoadError;
pub use encode::NvidiaEncoders;
pub use error::Error;
pub use jpeg::NvidiaJpegDecoder;
pub use motion::NvidiaMotion;
pub use nvdec::{ChromaFormat, DecodeCapabilities, NvdecCodec, SurfaceFormat, decode_capabilities};
pub use nvenc::encoder::{
    Bitstream, EncodeParams, EncodeProfile, EncodeProfileSupport, EncodeRateControl, NvencEncoder,
    Packet, PictureType, RawFrame, Tuning, encode_profiles,
};
pub use nvenc::max_api_version;
pub use probe::{EncodeCodecReport, EncoderReport, encoder_report};

/// `value` as a `usize`, which holds every `u32` on the 64-bit hosts the
/// crate runs on (asserted below).
pub(crate) fn usize_of(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

const _: () = assert!(usize::BITS >= u32::BITS);

/// The host's NVIDIA GPUs, as CUDA lists them; an error where the host has
/// no NVIDIA driver.
pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
    cuda::Cuda::get()?.devices()
}
