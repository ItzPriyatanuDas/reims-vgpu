//! The conformance suite on VideoToolbox: its command line
//! ([`reims_vgpu_video_conformance::cli`]), with DEVICE `hardware`, the one
//! device VideoToolbox has.
//!
//! ```sh
//! videotoolbox-conformance hardware decode-check STREAM REFERENCE...
//! ```

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    reims_vgpu_video_conformance::cli::run(
        "videotoolbox-conformance",
        &args,
        &|device| match device {
            "hardware" => Ok(reims_vgpu_video_videotoolbox::backend(
                reims_vgpu_video_core::runtime::StdRuntime::shared(),
            )),
            other => Err(format!(
                "VideoToolbox has one device, hardware, and not {other}"
            )),
        },
    )
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("the VideoToolbox conformance suite needs macOS");
    std::process::ExitCode::FAILURE
}
