//! VA-API through libva, with a binding of our own ([`ffi`]), and the
//! decoders on it. Linux only: lib.rs leaves this module out elsewhere.
//! libva is loaded at run time, not linked ([`libva`]), so a host without
//! it runs, and opens no display.
//!
//! Every `unsafe` block of the crate is here, each with the reason it is
//! sound, which the lints below hold it to; the binding's layouts and
//! constants are checked against libva's headers by `ffi`'s ignored tests.

#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

mod av1;
mod backend;
mod buffer;
mod calls;
mod caps;
mod display;
mod encoder;
mod fence;
mod ffi;
mod frame;
mod h264;
mod hevc;
mod host_memory;
mod image;
mod jpeg;
mod libva;
mod readback;
#[cfg(test)]
mod spin;
mod surface;
mod vp9;
mod vpp;
mod vpp_ffi;
mod wait;
mod whole;
mod zero_copy;

pub use av1::{AV1_MAIN, Av1Decoder};
pub use backend::VaapiBackend;
pub use caps::{
    Capabilities, ConfigCapabilities, Entrypoint, Fourcc, Profile, SurfaceCapabilities,
    proc_filter_name, rt_format_names,
};
pub use display::{Display, Error};
pub use encoder::{
    EncoderCapabilities, EncoderSupport, Feature, HevcBlockSizes, HevcFeatures, UploadMethod,
    VaapiEncoderBackend, VideoEncoder, encoder_support,
};
pub use frame::{Frame, ReadBack};
pub use h264::H264Decoder;
pub use hevc::HevcDecoder;
pub use jpeg::VaapiJpegDecoder;
pub use libva::LoadError;
pub use readback::{ReadbackTimes, ReadbackWay, read_back_by};
pub use vp9::{VP9_PROFILE0, VP9_PROFILE2, Vp9Decoder};
pub use whole::{NAME, backend_on, open_backend};
