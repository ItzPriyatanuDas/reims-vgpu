//! Decodes an MP4's H.264, HEVC, AV1 or VP9 track on NVDEC through the
//! backend with every sample damaged, and then again whole, to check that
//! damaged input fails cleanly: each error is the backend's, nothing
//! crashes or hangs, and the GPU decodes the whole stream right
//! afterwards.
//!
//! ```sh
//! nvidia-damage GPU FILE.mp4 MODE SEED
//! ```
//!
//! MODE is how each sample is damaged, with a generator seeded by SEED:
//! - `flip:N`: N bytes of the sample, anywhere, get random values;
//! - `tail:N`: N bytes past the sample's first 8 get random values, so
//!   its headers mostly read and its coded data does not;
//! - `cut:P`: the sample keeps its first P percent;
//! - `drop:P`: P percent of the samples are left out, references and all.
//!
//! The damaged pass's outcomes go to stderr: how many samples decoded, how
//! many failed and with which kind of error, how many frames came out and
//! copied, and the slowest decode and copy. The whole pass prints each
//! frame's MD5 to stdout, as nvidia-framemd5 does, for a comparison with
//! the stream's reference. Exits 1 when a call took longer than 5 s.

use std::process::ExitCode;
use std::time::Duration;

use reims_vgpu_video_conformance::md5::Md5;
use reims_vgpu_video_conformance::mp4;
use reims_vgpu_video_core::backend::{
    BackendError, Codec, CompressedFrame, DecodedFrame, DecoderConfig, PixelFormat,
};
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
use reims_vgpu_video_core::surfaces::PlaneDestination;
use reims_vgpu_video_core::wire::message::MediaTime;

/// A call slower than this is taken as a hang.
const HANG: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [gpu, path, mode, seed] = args.as_slice() else {
        eprintln!("usage: nvidia-damage GPU FILE.mp4 flip:N|tail:N|cut:P|drop:P SEED");
        return ExitCode::from(2);
    };
    let Some(mode) = Damage::parse(mode) else {
        eprintln!("nvidia-damage: {mode}: not flip:N, tail:N, cut:P or drop:P");
        return ExitCode::from(2);
    };
    let Ok(seed) = seed.parse::<u64>() else {
        eprintln!("nvidia-damage: {seed}: not a number");
        return ExitCode::from(2);
    };
    match run(gpu, path, mode, seed) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("nvidia-damage: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Clone, Copy)]
enum Damage {
    Flip(usize),
    Tail(usize),
    Cut(usize),
    Drop(u64),
}

impl Damage {
    fn parse(text: &str) -> Option<Self> {
        let (kind, n) = text.split_once(':')?;
        let n: usize = n.parse().ok()?;
        match kind {
            "flip" => Some(Self::Flip(n)),
            "tail" => Some(Self::Tail(n)),
            "cut" if n <= 100 => Some(Self::Cut(n)),
            "drop" if n <= 100 => Some(Self::Drop(n as u64)),
            _ => None,
        }
    }

    /// The sample as damaged, or None when it is dropped.
    fn apply(self, sample: &[u8], random: &mut Random) -> Option<Vec<u8>> {
        let mut out = sample.to_vec();
        match self {
            Self::Flip(n) | Self::Tail(n) => {
                let start = if matches!(self, Self::Tail(_)) { 8 } else { 0 };
                if out.len() > start {
                    for _ in 0..n {
                        let at = start + random.below(out.len() - start);
                        out[at] = random.next() as u8;
                    }
                }
            }
            Self::Cut(percent) => out.truncate(out.len() * percent / 100),
            Self::Drop(percent) => {
                if random.below(100) < percent as usize {
                    return None;
                }
            }
        }
        Some(out)
    }
}

/// xorshift64*: the damage is the same for a seed on every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// What the damaged pass saw.
#[derive(Default)]
struct Tally {
    decoded: usize,
    bad_data: usize,
    unsupported: usize,
    malfunction: usize,
    /// RASL pictures skipped after a CRA picture that ended a failure.
    skipped: usize,
    frames: usize,
    copied: usize,
    copy_errors: usize,
    slowest_decode: Duration,
    slowest_copy: Duration,
    first_errors: Vec<String>,
}

impl Tally {
    fn error(&mut self, what: &str, error: &BackendError) {
        match error {
            BackendError::BadData(_) => self.bad_data += 1,
            BackendError::Unsupported(_) => self.unsupported += 1,
            BackendError::SkippedLeadingPicture(_) => self.skipped += 1,
            _ => self.malfunction += 1,
        }
        if self.first_errors.len() < 3 {
            self.first_errors.push(format!("{what}: {error}"));
        }
    }
}

fn run(gpu: &str, path: &str, damage: Damage, seed: u64) -> Result<bool, String> {
    let file = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let (codec, track) = [
        (Codec::H264, mp4::h264_track as fn(&[u8]) -> _),
        (Codec::Hevc, mp4::hevc_track),
        (Codec::Av1, mp4::av1_track),
        (Codec::Vp9, mp4::vp9_track),
    ]
    .into_iter()
    .find_map(|(codec, track)| track(&file).ok().map(|t| (codec, t)))
    .ok_or_else(|| format!("{path}: no H.264, HEVC, AV1 or VP9 track"))?;
    let samples: Vec<&[u8]> = track
        .samples
        .iter()
        .map(|&(offset, size)| file.get(offset..offset + size).unwrap_or_default())
        .collect();
    let backend = reims_vgpu_video_nvidia::open_backend(gpu, StdRuntime::shared())
        .map_err(|e| e.to_string())?;
    let decoders = backend.decoders.ok_or("no decoders")?;
    let config = DecoderConfig {
        codec,
        width: track.size.0,
        height: track.size.1,
        configuration: track.avcc.clone(),
    };

    let mut random = Random(seed.max(1));
    let mut tally = Tally::default();
    let mut decoder = decoders.open(&config).map_err(|e| e.to_string())?;
    let mut sent = 0;
    for (i, sample) in samples.iter().enumerate() {
        let Some(damaged) = damage.apply(sample, &mut random) else {
            continue;
        };
        sent += 1;
        let started = StdRuntime.now();
        let result = decoder.decode(&frame(i, &damaged));
        tally.slowest_decode = tally.slowest_decode.max(StdRuntime.elapsed_since(started));
        match result {
            Ok(frames) => {
                tally.decoded += 1;
                copy_all(frames, &mut tally);
            }
            Err(error) => tally.error(&format!("sample {i}"), &error),
        }
    }
    match decoder.flush() {
        Ok(frames) => copy_all(frames, &mut tally),
        Err(error) => tally.error("flush", &error),
    }
    drop(decoder);
    eprintln!(
        "damaged: {sent} samples sent, {} decoded, {} bad data, {} unsupported, {} malfunction, \
         {} skipped; {} frames, {} copied, {} copies failed; slowest decode {:.1} ms, copy {:.1} ms",
        tally.decoded,
        tally.bad_data,
        tally.unsupported,
        tally.malfunction,
        tally.skipped,
        tally.frames,
        tally.copied,
        tally.copy_errors,
        tally.slowest_decode.as_secs_f64() * 1e3,
        tally.slowest_copy.as_secs_f64() * 1e3,
    );
    for error in &tally.first_errors {
        eprintln!("  {error}");
    }

    // The whole stream again, on a decoder of its own.
    let mut decoder = decoders.open(&config).map_err(|e| e.to_string())?;
    let mut index = 0;
    for (i, sample) in samples.iter().enumerate() {
        let frames = decoder
            .decode(&frame(i, sample))
            .map_err(|e| format!("whole pass, sample {i}: {e}"))?;
        print(frames, &mut index)?;
    }
    print(decoder.flush().map_err(|e| e.to_string())?, &mut index)?;
    eprintln!("whole: {} samples, {index} frames", samples.len());
    let hung = tally.slowest_decode > HANG || tally.slowest_copy > HANG;
    if hung {
        eprintln!("a call took longer than {} s", HANG.as_secs());
    }
    Ok(!hung)
}

fn frame<'a>(i: usize, data: &'a [u8]) -> CompressedFrame<'a> {
    CompressedFrame {
        frame_id: i64::try_from(i).unwrap_or(i64::MAX),
        data,
        sample_sizes: &[],
        presentation_time: MediaTime::INVALID,
        decode_time: MediaTime::INVALID,
        duration: MediaTime::INVALID,
        new_config: None,
    }
}

fn copy_all(frames: Vec<Box<dyn DecodedFrame>>, tally: &mut Tally) {
    for frame in frames {
        tally.frames += 1;
        let started = StdRuntime.now();
        let result = packed(frame.as_ref());
        tally.slowest_copy = tally.slowest_copy.max(StdRuntime.elapsed_since(started));
        match result {
            Ok(_) => tally.copied += 1,
            Err(error) => {
                tally.copy_errors += 1;
                tally.error(&format!("copy of frame {}", frame.frame_id()), &error);
            }
        }
    }
}

fn print(frames: Vec<Box<dyn DecodedFrame>>, index: &mut usize) -> Result<(), String> {
    for frame in frames {
        let bytes = packed(frame.as_ref()).map_err(|e| e.to_string())?;
        let mut md5 = Md5::new();
        md5.update(&bytes);
        println!(
            "frame {index} id {} {}x{} {}",
            frame.frame_id(),
            frame.width(),
            frame.height(),
            md5.hex()
        );
        *index += 1;
    }
    Ok(())
}

/// The frame copied out as NV12 or P010 at its own size.
fn packed(frame: &dyn DecodedFrame) -> Result<Vec<u8>, BackendError> {
    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let sample = match frame.pixel_format() {
        PixelFormat::P010VideoRange | PixelFormat::P010FullRange => 2,
        _ => 1,
    };
    let chroma_height = height.div_ceil(2);
    let luma_row = width * sample;
    let chroma_row = width.div_ceil(2) * 2 * sample;
    let mut luma = vec![0u8; luma_row * height];
    let mut chroma = vec![0u8; chroma_row * chroma_height];
    frame.read_back()?.copy_to(&mut [
        PlaneDestination {
            bytes: &mut luma,
            bytes_per_row: luma_row,
            rows: u32::try_from(height).unwrap_or(0),
        },
        PlaneDestination {
            bytes: &mut chroma,
            bytes_per_row: chroma_row,
            rows: u32::try_from(chroma_height).unwrap_or(0),
        },
    ])?;
    luma.extend_from_slice(&chroma);
    Ok(luma)
}
