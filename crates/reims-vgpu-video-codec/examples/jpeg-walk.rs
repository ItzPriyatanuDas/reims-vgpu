//! Walks each JPEG's scan as the JPEG engine does before a decode
//! (`reims_vgpu_video_codec::jpeg::walk`), and says what the engine would
//! answer and how long the walk took.
//!
//! ```sh
//! jpeg-walk [--repeat N] FILE.jpg...
//! ```
//!
//! One line a file: `walk NAME: decodes`, with its MCUs, restarts and the
//! walk's mean time in all and per megapixel, or `walk NAME: BadMedia` or
//! `DeviceError`, with why, as the engine's status for it would be.

use std::process::ExitCode;

use reims_vgpu_video_codec::jpeg::{self, Error};
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    let mut repeat = 1u32;
    if args.peek().map(String::as_str) == Some("--repeat") {
        args.next();
        match args.next().and_then(|n| n.parse().ok()) {
            Some(n) if n > 0 => repeat = n,
            _ => {
                eprintln!("usage: jpeg-walk [--repeat N] FILE.jpg...");
                return ExitCode::from(2);
            }
        }
    }
    let files: Vec<String> = args.collect();
    if files.is_empty() {
        eprintln!("usage: jpeg-walk [--repeat N] FILE.jpg...");
        return ExitCode::from(2);
    }
    for file in files {
        let name = file.rsplit('/').next().unwrap_or(&file).to_owned();
        let data = match std::fs::read(&file) {
            Ok(data) => data,
            Err(error) => {
                println!("walk {name}: unreadable: {error}");
                continue;
            }
        };
        let image = match jpeg::parse(&data) {
            Ok(image) => image,
            Err(error) => {
                println!("walk {name}: {} (parse): {error}", status(&error));
                continue;
            }
        };
        let started = StdRuntime.now();
        let mut result = jpeg::walk(&image);
        for _ in 1..repeat {
            result = jpeg::walk(&image);
        }
        let seconds = StdRuntime.elapsed_since(started).as_secs_f64() / f64::from(repeat);
        let megapixels = f64::from(image.frame.width) * f64::from(image.frame.height) / 1_000_000.0;
        match result {
            Ok(walked) => println!(
                "walk {name}: decodes, {}x{}, {} bytes of scan, {} MCUs, {} restarts, \
                 {:.3} ms, {:.3} ms per megapixel",
                image.frame.width,
                image.frame.height,
                image.entropy_coded_data.len(),
                walked.mcus,
                walked.restarts,
                seconds * 1000.0,
                seconds * 1000.0 / megapixels
            ),
            Err(error) => println!("walk {name}: {}: {error}", status(&error)),
        }
    }
    ExitCode::SUCCESS
}

/// The engine's status for an image the parser or the walk refuses.
fn status(error: &Error) -> &'static str {
    match error {
        Error::ScanEndsEarly => "DeviceError",
        Error::Unsupported(_) => "BadMedia (unsupported)",
        _ => "BadMedia",
    }
}
