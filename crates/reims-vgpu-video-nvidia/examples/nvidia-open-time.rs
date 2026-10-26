//! Times how long the NVIDIA backend takes to open a decoder and an
//! encoder, as the host session opens one at a guest's `dsta` or `esta`:
//! the backend opened once, as QEMU opens it, then ROUNDS rounds, each
//! opening and dropping an H.264 decoder of FILE.mp4's configuration and
//! an H.264 and an HEVC encoder at its size, GAP seconds apart. A guest
//! gives up on a session whose open takes over about a second, so the slow
//! ones are the finding.
//!
//! ```sh
//! nvidia-open-time GPU FILE.mp4 ROUNDS GAP
//! ```
//!
//! Prints a line an open: the round, what was opened, and the milliseconds
//! the open and the drop took.

use std::process::ExitCode;
use std::time::Duration;

use reims_vgpu_video_conformance::mp4;
use reims_vgpu_video_core::backend::{Codec, DecoderConfig};
use reims_vgpu_video_core::encoder_backend::{EncoderConfig, EncoderSettings};
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [gpu, path, rounds, gap] = args.as_slice() else {
        eprintln!("usage: nvidia-open-time GPU FILE.mp4 ROUNDS GAP");
        return ExitCode::from(2);
    };
    let (Ok(rounds), Ok(gap)) = (rounds.parse::<u32>(), gap.parse::<f64>()) else {
        eprintln!("nvidia-open-time: ROUNDS and GAP are numbers");
        return ExitCode::from(2);
    };
    match run(gpu, path, rounds, Duration::from_secs_f64(gap.max(0.0))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nvidia-open-time: {e}");
            ExitCode::FAILURE
        }
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn run(gpu: &str, path: &str, rounds: u32, gap: Duration) -> Result<(), String> {
    let file = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let track = mp4::h264_track(&file).map_err(|e| format!("{path}: {e}"))?;
    let started = StdRuntime.now();
    let backend = reims_vgpu_video_nvidia::open_backend(gpu, StdRuntime::shared())
        .map_err(|e| e.to_string())?;
    println!(
        "backend opened in {:.1} ms",
        milliseconds(StdRuntime.elapsed_since(started))
    );
    let decoders = backend.decoders.ok_or("no decoders")?;
    let encoders = backend.encoders.ok_or("no encoders")?;
    let (width, height) = track.size;
    let decoder_config = DecoderConfig {
        codec: Codec::H264,
        width,
        height,
        configuration: track.avcc.clone(),
    };
    for round in 0..rounds {
        if round > 0 {
            std::thread::sleep(gap);
        }
        let started = StdRuntime.now();
        let decoder = decoders.open(&decoder_config).map_err(|e| e.to_string())?;
        let opened = StdRuntime.elapsed_since(started);
        let started = StdRuntime.now();
        drop(decoder);
        println!(
            "round {round} decoder H264 {width}x{height}: open {:.1} ms, drop {:.1} ms",
            milliseconds(opened),
            milliseconds(StdRuntime.elapsed_since(started))
        );
        for codec in [Codec::H264, Codec::Hevc] {
            let config = EncoderConfig {
                codec,
                width,
                height,
                settings: EncoderSettings::default(),
            };
            let started = StdRuntime.now();
            let encoder = encoders.open(&config).map_err(|e| e.to_string())?;
            let opened = StdRuntime.elapsed_since(started);
            let started = StdRuntime.now();
            drop(encoder);
            println!(
                "round {round} encoder {codec:?} {width}x{height}: open {:.1} ms, drop {:.1} ms",
                milliseconds(opened),
                milliseconds(StdRuntime.elapsed_since(started))
            );
        }
    }
    Ok(())
}
