//! The conformance suite on NVIDIA: its command line
//! ([`reims_vgpu_video_conformance::cli`]), with DEVICE a CUDA ordinal, a
//! PCI bus ID or a DRM node.
//!
//! ```sh
//! nvidia-conformance 0 decode-check STREAM REFERENCE...
//! ```

use reims_vgpu_video_core::runtime::StdRuntime;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    reims_vgpu_video_conformance::cli::run("nvidia-conformance", &args, &|gpu| {
        reims_vgpu_video_nvidia::open_backend(gpu, StdRuntime::shared()).map_err(|e| e.to_string())
    })
}
