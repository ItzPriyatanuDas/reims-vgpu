//! The behaviour the guest observes of the VideoToolbox device (106b:1a03)
//! and the M2 scaler device (106b:1a0b), generic over the host's backends:
//! the `vtpv` messages, the host session and its sessions, and the scaler's
//! requests.
//!
//! - [`host::HostSession`] is the VideoToolbox device's host side: the
//!   device delivers each guest message to it and rings what it sends
//!   through its [`host::GuestSink`] onto the guest's queue.
//! - [`surfaces::Surfaces`] is how it reaches guest surfaces, by mapping
//!   ID; [`backend::DecoderBackend`] is how it decodes, and
//!   [`encoder_backend::EncoderBackend`] how it encodes,
//!   [`motion::MotionEstimationBackend`] how it estimates motion, and
//!   [`jpeg::JpegDecoder`] how its JPEG engine decodes.
//! - [`device_memory`] is how a backend's device writes a guest surface's
//!   pages itself, where the host lends them as a dma-buf or a view, and
//!   [`device_wait`] how long a backend waits for its device, and what it
//!   keeps of work the device has not finished by then.
//! - [`wire`] reads and writes the device's messages.
//! - [`status`] has the status codes the guest sees.
//! - [`runtime`] is the threads and the clock the host gives the video
//!   crates, and [`sync`] the locks they take.

// The library's lint set, reims-vgpu-core's and the codec crate's: no
// unsafe code but what is marked, no indexing that can panic, and no silent
// `as` conversion outside tests, where clippy.toml lets code unwrap, index
// and panic. The reference scaler holds its own exceptions (its
// attributes).
#![deny(
    unsafe_code,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::mod_module_files
)]
#![cfg_attr(
    not(test),
    deny(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )
)]

pub mod backend;
pub mod device_memory;
pub mod device_wait;
pub mod encoder_backend;
pub mod fourcc;
#[cfg_attr(
    test,
    expect(
        unsafe_code,
        reason = "the host session's tests lend stand-in surfaces"
    )
)]
pub mod host;
pub mod jpeg;
pub mod motion;
mod num;
pub mod runtime;
pub mod scaler;
pub mod status;
pub mod surfaces;
pub mod sync;
pub mod wire;

/// The property lists the API carries ([`encoder_backend::EncoderSettings`]
/// and [`encoder_backend::EncoderBackend::check_property`] among them), so
/// that a host names the same crate and version.
pub use plist;
