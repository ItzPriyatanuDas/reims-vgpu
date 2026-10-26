//! The conformance suite on VA-API: its command line
//! ([`reims_vgpu_video_conformance::cli`]), with DEVICE a render node. The
//! tools' checks run it: `framemd5` for tools/host-session-check.sh and
//! tools/perf-check.sh, `jpeg-check` for tools/jpeg-check.sh.
//! `RVGV_VAAPI_READBACK` picks the way frames are read back
//! (examples/common/readback.rs).
//!
//! ```sh
//! vaapi-conformance /dev/dri/renderD128 decode-check STREAM REFERENCE...
//! vaapi-conformance /dev/dri/renderD128 framemd5 STREAM [--async] [--sequence FILE]
//! ```

#[cfg(target_os = "linux")]
#[path = "common/readback.rs"]
mod readback;
#[cfg(target_os = "linux")]
use reims_vgpu_video_core::runtime::StdRuntime;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    readback::read_back_as_asked();
    let args: Vec<String> = std::env::args().skip(1).collect();
    reims_vgpu_video_conformance::cli::run("vaapi-conformance", &args, &|node| {
        reims_vgpu_video_vaapi::open_backend(node, StdRuntime::shared()).map_err(|e| e.to_string())
    })
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("the VA-API conformance suite needs Linux and libva");
    std::process::ExitCode::FAILURE
}
