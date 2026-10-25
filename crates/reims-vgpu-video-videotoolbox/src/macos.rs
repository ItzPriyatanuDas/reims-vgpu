//! The backend on macOS (lib.rs). Every `unsafe` block of the crate is
//! under here, each with the reason it is sound.

mod capabilities;
mod cf;
mod decoder;
mod encoder;
mod ffi;
mod motion;
mod pixel_buffer;
mod session;
mod status;
mod stream_format;
#[cfg(test)]
mod tests;
mod whole;
mod zero_copy;

pub use decoder::{Decoder, DecoderChoice, VideoToolboxBackend};
pub use encoder::VideoToolboxEncoderBackend;
pub use motion::VideoToolboxMotionBackend;
pub use whole::{NAME, backend};
