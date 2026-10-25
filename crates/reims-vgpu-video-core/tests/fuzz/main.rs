//! Seeded fuzzers of every reader of guest input in core: the wire layer,
//! the host session over stand-in backends, and the scaler's requests.
//!
//! Each target has a quick run, which `make check` makes, and a long one,
//! ignored, sized by `RVGV_FUZZ_CASES`, `RVGV_FUZZ_SECONDS` and
//! `RVGV_FUZZ_SEED`:
//!
//! ```sh
//! RVGV_FUZZ_CASES=1000000 cargo test --offline -p reims-vgpu-video-core \
//!     --test fuzz -- --ignored
//! ```
//!
//! A failure prints the seed and the input's index, and the command that
//! replays that input alone.

mod generate;
mod host;
mod motion;
mod scaler;
mod support;
mod wire;

/// Every allocation of this binary is counted, so that each input is held
/// to a memory bound.
#[global_allocator]
static ALLOCATOR: support::CountingAllocator = support::CountingAllocator;
