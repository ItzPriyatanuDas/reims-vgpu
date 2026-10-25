//! One conformance suite for every backend. A stand-in for the guest
//! drives core's devices on any [`Backend`] as a guest would, and checks
//! what comes back against references made elsewhere: a software decode's
//! frame hashes, the source frames, core's CPU scaler. What the guest sees
//! is core's, so a backend passes when it decodes, encodes and converts
//! what it lists.
//!
//! - [`decode`] decodes a stream through the host session, frame by frame;
//!   [`suite`] checks the frames against a reference list.
//! - [`encode`] encodes frames through the host session; [`encode_check`]
//!   draws frames, encodes them, checks the samples as a guest would take
//!   them, and decodes them back against their source.
//! - [`scaler`] runs the scaler's transforms against core's CPU reference.
//! - [`jpeg`] decodes a JPEG through the JPEG engine's decoder and
//!   compares its planes with a reference decoder's.
//! - [`cli`] is the command line each backend's `conformance` example
//!   runs, with the backend's own opener: the suite is the same on every
//!   backend, only the opening differs.
//! - [`mp4`], [`ivf`] and [`md5`] read the test streams and hash frames,
//!   and [`mp4_write`] writes what an encoder gave, with no dependency.
//! - [`planar`] packs a software decode's planar frames into NV12 and
//!   P010, for the references of the stand-in guest's frames.
//!
//! [`Backend`]: reims_vgpu_video_core::backend::Backend

pub mod cli;
pub mod decode;
pub mod encode;
pub mod encode_check;
mod engines;
#[cfg(not(target_os = "macos"))]
mod guest_pages;
#[cfg(target_os = "macos")]
mod guest_surface;
pub mod ivf;
pub mod jpeg;
pub mod md5;
pub mod motion;
pub mod mp4;
pub mod mp4_write;
pub mod planar;
pub mod scaler;
pub mod suite;
