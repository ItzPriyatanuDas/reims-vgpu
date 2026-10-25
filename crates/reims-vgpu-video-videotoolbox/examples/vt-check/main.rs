//! Checks the VideoToolbox backend's decoders on the host against reference
//! MD5 lists, frame by frame (tools/videotoolbox-check.sh runs it over the
//! test streams).
//!
//! ```sh
//! vt-check decode [--software] [--ref MD5S] FILE.mp4
//! vt-check list
//! ```
//!
//! `list` prints what the backend offers on the host, as VideoToolbox
//! answered: each decoder's profiles with their highest levels and its
//! largest size, and each encoder's profiles, size, source formats and
//! reordering.
//!
//! `decode` opens the backend's decoder (`Decoder::open`, the hardware
//! decoder, or VideoToolbox's software decoder with `--software`) on the
//! track's record and decodes each sample in decode order, as core's
//! decoder session does, but with no session between: the backend alone.
//! The conformance example runs the same streams through core's host
//! session, as a guest would.
//!
//! Each frame's MD5 is over its pixels: the luma rows, then the
//! interleaved chroma rows, each row's visible bytes only, two bytes a
//! sample for 10-bit formats. Frames are put in presentation order and
//! compared with MD5S, a line a frame, either `PTS MD5` or `MD5` alone.
//!
//! Prints `frame <n> pts <seconds> <md5>` for each frame in presentation
//! order, then `result: pass|fail (M of N frames match ...)`. Exit status
//! 0 when every frame matched (or, with no list, when every sample
//! decoded), 1 otherwise, 2 for a usage or file error.

#[cfg(target_os = "macos")]
mod mp4;

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    macos::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("vt-check needs macOS and VideoToolbox");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::process::ExitCode;

    use reims_vgpu_video_core::backend::DecoderBackend;
    use reims_vgpu_video_core::backend::{
        BackendDecoder, Codec, CompressedFrame, DecodedFrame, DecoderConfig,
    };
    use reims_vgpu_video_core::encoder_backend::EncoderBackend;
    use reims_vgpu_video_core::fourcc::FourCc;
    use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
    use reims_vgpu_video_core::surfaces::PlaneDestination;
    use reims_vgpu_video_core::wire::message::MediaTime;
    use reims_vgpu_video_videotoolbox::{
        Decoder, DecoderChoice, VideoToolboxBackend, VideoToolboxEncoderBackend,
    };

    use super::mp4::{self, Track};
    use reims_vgpu_video_conformance::md5::{Md5, self_test};

    /// One frame out: its sample, presentation time and MD5.
    pub(crate) struct Hashed {
        pub(crate) sample: usize,
        pub(crate) presentation_time: i64,
        pub(crate) md5: String,
    }

    /// The MD5 of a frame's pixels: tight luma rows then tight chroma
    /// rows.
    pub(crate) fn frame_md5(frame: &dyn DecodedFrame) -> Result<String, String> {
        let sample = frame.pixel_format().bytes_per_sample();
        let width = frame.width() as usize;
        let height = frame.height() as usize;
        let luma_row = width * sample;
        let chroma_row = width.div_ceil(2) * 2 * sample;
        let chroma_rows = height.div_ceil(2);
        let mut luma = vec![0u8; luma_row * height];
        let mut chroma = vec![0u8; chroma_row * chroma_rows];
        let mut planes = [
            PlaneDestination {
                bytes: &mut luma,
                bytes_per_row: luma_row,
                rows: frame.height(),
            },
            PlaneDestination {
                bytes: &mut chroma,
                bytes_per_row: chroma_row,
                rows: u32::try_from(chroma_rows).unwrap_or(0),
            },
        ];
        let copying = StdRuntime.now();
        (frame.read_back().and_then(|read| read.copy_to(&mut planes)))
            .map_err(|e| e.to_string())?;
        let copied = StdRuntime.elapsed_since(copying);
        let hashing = StdRuntime.now();
        let mut md5 = Md5::new();
        md5.update(&luma);
        md5.update(&chroma);
        let hex = md5.hex();
        if std::env::var_os("VT_CHECK_TIMES").is_some() {
            eprintln!(
                "copy {:.2} ms, md5 {:.2} ms",
                copied.as_secs_f64() * 1e3,
                StdRuntime.elapsed_since(hashing).as_secs_f64() * 1e3
            );
        }
        Ok(hex)
    }

    /// Core's codec of the track's sample entry.
    pub(crate) fn codec(track: &Track) -> Result<Codec, String> {
        let fourcc = match &track.codec {
            b"avc3" => *b"avc1",
            b"hev1" => *b"hvc1",
            other => *other,
        };
        Codec::from_fourcc(FourCc::new(fourcc))
            .ok_or_else(|| format!("core knows no {}", String::from_utf8_lossy(&track.codec)))
    }

    /// A sample's time in the track's timescale as core's media time.
    pub(crate) fn time(value: i64, timescale: u32) -> MediaTime {
        MediaTime {
            value,
            timescale: timescale.cast_signed(),
            flags: MediaTime::FLAG_VALID,
            epoch: 0,
        }
    }

    /// Decodes every sample through the backend's decoder, as core's
    /// session drives it: one `decode` a sample, then `flush`.
    fn decode(file: &[u8], track: &Track, choice: DecoderChoice) -> Result<Vec<Hashed>, String> {
        let config = DecoderConfig {
            codec: codec(track)?,
            width: track.size.0,
            height: track.size.1,
            configuration: track.record.clone(),
        };
        let mut decoder = Decoder::open(&config, choice).map_err(|e| e.to_string())?;
        eprintln!("output format: {:?}", decoder.output_format());
        let mut hashed = Vec::new();
        let mut keep = |frames: Vec<Box<dyn DecodedFrame>>| -> Result<(), String> {
            for frame in frames {
                let sample = usize::try_from(frame.frame_id()).map_err(|e| e.to_string())?;
                let presentation_time = track
                    .samples
                    .get(sample)
                    .ok_or("a frame of no sample")?
                    .presentation_time;
                hashed.push(Hashed {
                    sample,
                    presentation_time,
                    md5: frame_md5(frame.as_ref())?,
                });
            }
            Ok(())
        };
        let start = StdRuntime.now();
        let mut failed = 0;
        for (index, sample) in track.samples.iter().enumerate() {
            let data = file
                .get(sample.offset..sample.offset + sample.size)
                .ok_or("a sample past the file's end")?;
            let frame = CompressedFrame {
                frame_id: index as i64,
                data,
                sample_sizes: &[],
                presentation_time: time(sample.presentation_time, track.timescale),
                decode_time: time(sample.decode_time, track.timescale),
                duration: time(sample.duration, track.timescale),
                new_config: None,
            };
            match decoder.decode(&frame) {
                Ok(frames) => {
                    if frames.len() != 1 {
                        eprintln!("sample {index}: {} frames out", frames.len());
                    }
                    keep(frames)?;
                }
                Err(error) => {
                    failed += 1;
                    eprintln!("sample {index}: {error}");
                }
            }
        }
        keep(decoder.flush().map_err(|e| e.to_string())?)?;
        let seconds = StdRuntime.elapsed_since(start).as_secs_f64();
        eprintln!(
            "decoded {} of {} samples ({failed} failed) in {seconds:.3} s, {:.1} fps",
            hashed.len(),
            track.samples.len(),
            hashed.len() as f64 / seconds
        );
        Ok(hashed)
    }

    /// The reference list: one MD5 a line, after its time where there is
    /// one.
    fn references(path: &str) -> Result<Vec<String>, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Ok(text
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .map(str::to_owned)
            .collect())
    }

    /// `vt-check list`.
    fn list() -> ExitCode {
        for codec in VideoToolboxBackend::new().capabilities() {
            let profiles: Vec<String> = codec
                .profiles
                .iter()
                .map(|p| match p.max_level {
                    Some(level) => format!("{} up to level {level}", p.profile),
                    None => format!("{}", p.profile),
                })
                .collect();
            println!(
                "decoder {:?}: profiles {}; up to {}x{}",
                codec.codec,
                profiles.join(", "),
                codec.max_width,
                codec.max_height
            );
        }
        for encoder in VideoToolboxEncoderBackend::new(StdRuntime::shared()).encoders() {
            println!(
                "encoder {:?}: profiles {:?}; up to {}x{}; sources {:?}; reordering {}",
                encoder.codec,
                encoder.profiles,
                encoder.max_width,
                encoder.max_height,
                encoder.source_formats,
                encoder.frame_reordering
            );
        }
        ExitCode::SUCCESS
    }

    pub(crate) fn main() -> ExitCode {
        if std::env::args().nth(1).as_deref() == Some("list") {
            return list();
        }
        if !self_test() {
            eprintln!("vt-check: the MD5 self-test failed");
            return ExitCode::from(2);
        }
        let mut args: Vec<String> = std::env::args().skip(1).collect();
        let take_flag = |args: &mut Vec<String>, flag: &str| {
            let at = args.iter().position(|arg| arg == flag);
            at.map(|at| args.remove(at)).is_some()
        };
        let reference = args.iter().position(|arg| arg == "--ref").and_then(|at| {
            let path = args.get(at + 1).cloned();
            args.drain(at..(at + 2).min(args.len()));
            path
        });
        let software = take_flag(&mut args, "--software");
        let (Some(mode), Some(path), None) = (args.first(), args.get(1), args.get(2)) else {
            eprintln!(
                "usage: vt-check decode [--software] [--ref MD5S] FILE.mp4\n       vt-check list"
            );
            return ExitCode::from(2);
        };
        let run = || -> Result<(Vec<Hashed>, usize), String> {
            let file = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let track = mp4::video_track(&file)?;
            eprintln!(
                "file: {path}, {}, {}x{}, {} samples",
                String::from_utf8_lossy(&track.codec),
                track.size.0,
                track.size.1,
                track.samples.len()
            );
            let hashed = match mode.as_str() {
                "decode" => {
                    let choice = if software {
                        DecoderChoice::Software
                    } else {
                        DecoderChoice::Hardware
                    };
                    decode(&file, &track, choice)?
                }
                other => return Err(format!("no mode {other}")),
            };
            Ok((hashed, track.samples.len()))
        };
        let (mut hashed, samples) = match run() {
            Ok(done) => done,
            Err(error) => {
                eprintln!("vt-check: {error}");
                return ExitCode::from(2);
            }
        };
        hashed.sort_by_key(|frame| (frame.presentation_time, frame.sample));
        for (index, frame) in hashed.iter().enumerate() {
            println!(
                "frame {index} sample {} pts {} {}",
                frame.sample, frame.presentation_time, frame.md5
            );
        }
        let Some(reference) = reference else {
            let all = hashed.len() == samples;
            println!(
                "result: {} ({} frames of {samples} samples; no reference)",
                if all { "pass" } else { "fail" },
                hashed.len()
            );
            return if all {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        };
        let expected = match references(&reference) {
            Ok(expected) => expected,
            Err(error) => {
                eprintln!("vt-check: {error}");
                return ExitCode::from(2);
            }
        };
        let matched = hashed
            .iter()
            .zip(&expected)
            .filter(|(frame, md5)| frame.md5 == **md5)
            .count();
        for (index, (frame, md5)) in hashed.iter().zip(&expected).enumerate() {
            if frame.md5 != *md5 {
                println!(
                    "differ frame {index} sample {}: {} reference {md5}",
                    frame.sample, frame.md5
                );
            }
        }
        let pass = matched == expected.len() && hashed.len() == expected.len();
        println!(
            "result: {} ({matched} of {} frames match {reference}; {} decoded)",
            if pass { "pass" } else { "fail" },
            expected.len(),
            hashed.len()
        );
        if pass {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}
