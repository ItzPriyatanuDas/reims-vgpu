//! Decodes an MP4's H.264, HEVC, AV1 or VP9 track on NVDEC through the
//! backend's decoders, without the host session, and prints each frame's
//! size and MD5 at its own size, so that a stream whose frames change size
//! (VP9's resizing, AV1's) is checked whole:
//!
//! ```sh
//! nvidia-framemd5 GPU FILE.mp4 [--planar | --monochrome] [--raw OUT] [--keep-going]
//! ```
//!
//! Each line is `frame INDEX id SAMPLE WIDTHxHEIGHT MD5`, in the order the
//! frames come out. The MD5 is over the frame packed as the guest's buffer
//! holds it (NV12 or P010, as `ffmpeg -pix_fmt nv12` or `p010le` hashes
//! it), or with `--planar` over the planar layout a software decoder
//! writes (`yuv420p`, or `yuv420p10le` with each sample in its low bits),
//! which ffmpeg hashes without converting anything: exact for odd sizes
//! too. `--monochrome` hashes the planar luma alone, as ffmpeg's `gray` and `gray10le`
//! do, and fails a frame whose chroma is not neutral (128, or 512 at 10 bits). `--raw`
//! writes the bytes hashed, frame after frame, to OUT, for a comparison sample by sample
//! with a software decode's raw output. `--keep-going` prints `refused id SAMPLE: ERROR`
//! for a sample the backend refuses, as core's session emits it without an image, and
//! goes on; the run then exits 1.

use std::process::ExitCode;

use reims_vgpu_video_conformance::md5::Md5;
use reims_vgpu_video_conformance::mp4;
use reims_vgpu_video_core::backend::{
    Codec, CompressedFrame, DecodedFrame, DecoderConfig, PixelFormat,
};
use reims_vgpu_video_core::runtime::StdRuntime;
use reims_vgpu_video_core::surfaces::PlaneDestination;
use reims_vgpu_video_core::wire::message::MediaTime;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = || {
        eprintln!(
            "usage: nvidia-framemd5 GPU FILE.mp4 [--planar | --monochrome] [--raw OUT] [--keep-going]"
        );
        ExitCode::from(2)
    };
    let [gpu, path, options @ ..] = args.as_slice() else {
        return usage();
    };
    let (mut layout, mut raw, mut keep_going) = (Layout::Packed, None, false);
    let mut options = options.iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "--planar" => layout = Layout::Planar,
            "--monochrome" => layout = Layout::Luma,
            "--keep-going" => keep_going = true,
            "--raw" => match options.next() {
                Some(out) => raw = Some(out.clone()),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    match run(gpu, path, layout, raw.as_deref(), keep_going) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nvidia-framemd5: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What a frame's MD5 is over.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// NV12 or P010, as the guest's buffer holds it.
    Packed,
    /// Each plane on its own, in each sample's low bits.
    Planar,
    /// The planar luma alone.
    Luma,
}

fn run(
    gpu: &str,
    path: &str,
    hashed: Layout,
    raw: Option<&str>,
    keep_going: bool,
) -> Result<(), String> {
    let file = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let mut raw = match raw {
        Some(out) => Some(std::fs::File::create(out).map_err(|e| format!("{out}: {e}"))?),
        None => None,
    };
    let (codec, track) = [
        (Codec::H264, mp4::h264_track as fn(&[u8]) -> _),
        (Codec::Hevc, mp4::hevc_track),
        (Codec::Av1, mp4::av1_track),
        (Codec::Vp9, mp4::vp9_track),
    ]
    .into_iter()
    .find_map(|(codec, track)| track(&file).ok().map(|t| (codec, t)))
    .ok_or_else(|| format!("{path}: no H.264, HEVC, AV1 or VP9 track"))?;
    let backend = reims_vgpu_video_nvidia::open_backend(gpu, StdRuntime::shared())
        .map_err(|e| e.to_string())?;
    let decoders = backend.decoders.ok_or("no decoders")?;
    let config = DecoderConfig {
        codec,
        width: track.size.0,
        height: track.size.1,
        configuration: track.avcc.clone(),
    };
    let mut decoder = decoders.open(&config).map_err(|e| e.to_string())?;
    let (mut index, mut refused) = (0, 0);
    let mut print = |frames: Vec<Box<dyn DecodedFrame>>| -> Result<(), String> {
        for frame in frames {
            let bytes = layout(frame.as_ref(), hashed)?;
            if let Some(raw) = &mut raw {
                std::io::Write::write_all(raw, &bytes).map_err(|e| format!("--raw: {e}"))?;
            }
            let mut md5 = Md5::new();
            md5.update(&bytes);
            println!(
                "frame {index} id {} {}x{} {}",
                frame.frame_id(),
                frame.width(),
                frame.height(),
                md5.hex()
            );
            index += 1;
        }
        Ok(())
    };
    for (i, &(offset, size)) in track.samples.iter().enumerate() {
        let data = file
            .get(offset..offset + size)
            .ok_or_else(|| format!("sample {i} is past the file's end"))?;
        let frame = CompressedFrame {
            frame_id: i64::try_from(i).unwrap_or(i64::MAX),
            data,
            sample_sizes: &[],
            presentation_time: MediaTime::INVALID,
            decode_time: MediaTime::INVALID,
            duration: MediaTime::INVALID,
            new_config: None,
        };
        match decoder.decode(&frame) {
            Ok(frames) => print(frames)?,
            Err(e) if keep_going => {
                println!("refused id {i}: {e}");
                refused += 1;
            }
            Err(e) => return Err(format!("sample {i}: {e}")),
        }
    }
    print(decoder.flush().map_err(|e| format!("flush: {e}"))?)?;
    eprintln!(
        "{} samples, {index} frames, {refused} refused",
        track.samples.len()
    );
    if refused > 0 {
        return Err(format!("{refused} samples refused"));
    }
    Ok(())
}

/// The frame copied out packed at its own size, as hashed in `hashed`.
fn layout(frame: &dyn DecodedFrame, hashed: Layout) -> Result<Vec<u8>, String> {
    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let sample = match frame.pixel_format() {
        PixelFormat::P010VideoRange | PixelFormat::P010FullRange => 2,
        _ => 1,
    };
    let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
    let luma_row = width * sample;
    let chroma_row = chroma_width * 2 * sample;
    let mut luma = vec![0u8; luma_row * height];
    let mut chroma = vec![0u8; chroma_row * chroma_height];
    frame
        .read_back()
        .map_err(|e| e.to_string())?
        .copy_to(&mut [
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
        ])
        .map_err(|e| e.to_string())?;
    if hashed == Layout::Packed {
        luma.extend_from_slice(&chroma);
        return Ok(luma);
    }
    // 16-bit samples hold P010's value in their high bits.
    let low_bits = |bytes: &[u8]| -> Vec<u8> {
        if sample == 1 {
            return bytes.to_vec();
        }
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&s| (u16::from_le_bytes(s) >> 6).to_le_bytes())
            .collect()
    };
    let mut out = low_bits(&luma);
    if hashed == Layout::Luma {
        let neutral: u16 = if sample == 1 { 128 } else { 512 << 6 };
        let samples: Vec<u16> = if sample == 1 {
            chroma.iter().map(|&s| u16::from(s)).collect()
        } else {
            chroma
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&s| u16::from_le_bytes(s))
                .collect()
        };
        if let Some(position) = samples.iter().position(|&s| s != neutral) {
            return Err(format!(
                "a monochrome frame's chroma sample {position} is {}, not {neutral}",
                samples[position]
            ));
        }
        return Ok(out);
    }
    for component in 0..2 {
        let mut plane = Vec::with_capacity(chroma_width * chroma_height * sample);
        for row in chroma.chunks_exact(chroma_row) {
            for pair in row.chunks_exact(2 * sample) {
                plane.extend_from_slice(&pair[component * sample..(component + 1) * sample]);
            }
        }
        out.extend(low_bits(&plane));
    }
    Ok(out)
}
