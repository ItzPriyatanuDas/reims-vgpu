//! Encodes raw frames through core's encoder backend on NVENC, as the host
//! session drives it for a guest: [`NvidiaEncoders`] opens the codec
//! crate's session encoder with the guest's settings, each frame goes in
//! as a source frame with its time stamps, and the samples come back with
//! their decode times, sync flags and configuration record. Writes what the
//! guest would get, so that ffmpeg can check it:
//!
//! - PREFIX.mp4, the samples behind the first configuration record;
//! - PREFIX.264 or PREFIX.265, Annex B, each record's parameter sets before
//!   the samples it covers;
//! - PREFIX.log, a line a sample in decode order;
//! - a summary on stderr: frames, records, bit/s and fps.
//!
//! ```sh
//! nvidia-session-encode GPU INPUT WxH PREFIX [--codec avc1|hvc1]
//!     [--format 420v|420f|x420|xf20] [--fps N] [--frames N] [--timescale T]
//!     [--profile N] [--level N] [--bitrate BPS] [--max-bitrate BPS]
//!     [--quality Q] [--keyint N] [--keyint-seconds S] [--reorder]
//!     [--cavlc|--cabac] [--colour P,T,M] [--force FRAME]...
//!     [--bitrate-at FRAME:BPS]
//! ```
//!
//! GPU is a CUDA ordinal, a PCI bus ID or a DRM node. INPUT is raw frames,
//! NV12 for `420v` and `420f`, P010 for `x420` and `xf20`.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;

use reims_vgpu_video_codec::encode::nal::length_prefixed_units;
use reims_vgpu_video_conformance::mp4_write as mp4;
use reims_vgpu_video_core::backend::{Codec, PixelFormat};
use reims_vgpu_video_core::encoder_backend::{
    ColourDescription, EncodedFrame, EncoderBackend, EncoderConfig, EncoderSettings, EntropyMode,
    SourceFrame,
};
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
use reims_vgpu_video_core::surfaces::PlaneSource;
use reims_vgpu_video_core::wire::message::MediaTime;
use reims_vgpu_video_nvidia::{Gpu, GpuSelector, NvidiaEncoders};

type Failure = Box<dyn std::error::Error>;

struct Options {
    gpu: GpuSelector,
    input: String,
    width: u32,
    height: u32,
    prefix: String,
    codec: Codec,
    format: PixelFormat,
    fps: u32,
    frames: Option<u64>,
    timescale: i32,
    settings: EncoderSettings,
    force: Vec<u64>,
    bitrate_at: Option<(u64, u32)>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nvidia-session-encode: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse() -> Result<Options, Failure> {
    let mut args = std::env::args().skip(1);
    let mut next = |what: &str| args.next().ok_or_else(|| format!("missing {what}"));
    let gpu = GpuSelector::parse(&next("GPU")?);
    let input = next("INPUT")?;
    let size = next("WxH")?;
    let (width, height) = size.split_once('x').ok_or("WxH wants a size")?;
    let prefix = next("PREFIX")?;
    let mut options = Options {
        gpu,
        input,
        width: width.parse()?,
        height: height.parse()?,
        prefix,
        codec: Codec::H264,
        format: PixelFormat::Nv12VideoRange,
        fps: 30,
        frames: None,
        timescale: 90_000,
        settings: EncoderSettings::default(),
        force: Vec::new(),
        bitrate_at: None,
    };
    while let Ok(flag) = next("an option") {
        let s = &mut options.settings;
        match flag.as_str() {
            "--codec" => {
                options.codec = match next("--codec")?.as_str() {
                    "avc1" => Codec::H264,
                    "hvc1" => Codec::Hevc,
                    other => return Err(format!("no codec {other}").into()),
                }
            }
            "--format" => {
                let name = next("--format")?;
                let bytes: [u8; 4] = name
                    .as_bytes()
                    .try_into()
                    .map_err(|_| "a format is 4 letters")?;
                options.format =
                    PixelFormat::from_fourcc(reims_vgpu_video_core::fourcc::FourCc::new(bytes))
                        .ok_or_else(|| format!("no format {name}"))?;
            }
            "--fps" => options.fps = next("--fps")?.parse()?,
            "--frames" => options.frames = Some(next("--frames")?.parse()?),
            "--timescale" => options.timescale = next("--timescale")?.parse()?,
            "--profile" => s.profile = Some(next("--profile")?.parse()?),
            "--level" => s.level = Some(next("--level")?.parse()?),
            "--bitrate" => s.average_bit_rate = Some(next("--bitrate")?.parse()?),
            "--max-bitrate" => s.max_bit_rate = Some(next("--max-bitrate")?.parse()?),
            "--quality" => s.quality = Some(next("--quality")?.parse()?),
            "--keyint" => s.max_key_frame_interval = Some(next("--keyint")?.parse()?),
            "--keyint-seconds" => {
                s.max_key_frame_interval_duration = Some(next("--keyint-seconds")?.parse()?);
            }
            "--reorder" => s.allow_frame_reordering = true,
            "--cavlc" => s.entropy = Some(EntropyMode::Cavlc),
            "--cabac" => s.entropy = Some(EntropyMode::Cabac),
            "--colour" => {
                let values: Vec<u8> = next("--colour")?
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<_, _>>()?;
                let [primaries, transfer, matrix] = values[..] else {
                    return Err("--colour wants P,T,M".into());
                };
                s.colour = ColourDescription {
                    primaries: Some(primaries),
                    transfer: Some(transfer),
                    matrix: Some(matrix),
                };
            }
            "--force" => options.force.push(next("--force")?.parse()?),
            "--bitrate-at" => {
                let text = next("--bitrate-at")?;
                let (frame, rate) = text.split_once(':').ok_or("--bitrate-at wants N:BPS")?;
                options.bitrate_at = Some((frame.parse()?, rate.parse()?));
            }
            other => return Err(format!("no option {other}").into()),
        }
    }
    options.settings.expected_frame_rate = Some(f64::from(options.fps));
    Ok(options)
}

fn time(value: i64, timescale: i32) -> MediaTime {
    MediaTime {
        value,
        timescale,
        flags: MediaTime::FLAG_VALID,
        epoch: 0,
    }
}

/// What came back, with the record it follows.
struct Emitted {
    frame: EncodedFrame,
    record: usize,
}

fn run() -> Result<(), Failure> {
    let options = parse()?;
    let gpu = Arc::new(Gpu::open(&options.gpu)?);
    let backend = NvidiaEncoders::new(gpu)?;
    eprintln!("encoders: {:?}", backend.encoders());
    let mut encoder = backend.open(&EncoderConfig {
        codec: options.codec,
        width: options.width,
        height: options.height,
        settings: options.settings.clone(),
    })?;
    let sample = options.format.bytes_per_sample();
    let width = usize::try_from(options.width)?;
    let height = usize::try_from(options.height)?;
    let stride = width * sample;
    let luma_bytes = stride * height;
    let mut frame = vec![0u8; luma_bytes + stride * height.div_ceil(2)];
    let mut input = File::open(&options.input)?;
    let ticks = i64::from(options.timescale) / i64::from(options.fps.max(1));
    let mut records: Vec<Vec<u8>> = Vec::new();
    let mut emitted: Vec<Emitted> = Vec::new();
    let mut gather = |frames: Vec<EncodedFrame>, record: Vec<u8>| {
        if records.last() != Some(&record) {
            records.push(record);
        }
        let index = records.len() - 1;
        for frame in frames {
            emitted.push(Emitted {
                frame,
                record: index,
            });
        }
    };
    let start = StdRuntime.now();
    let mut sent = 0u64;
    while options.frames.is_none_or(|limit| sent < limit) {
        match input.read_exact(&mut frame) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        if let Some((at, rate)) = options.bitrate_at
            && at == sent
        {
            let mut settings = options.settings.clone();
            settings.average_bit_rate = Some(rate);
            settings.max_bit_rate = settings.max_bit_rate.map(|_| rate.saturating_mul(3) / 2);
            encoder.reconfigure(&settings)?;
        }
        let (luma, chroma) = frame.split_at(luma_bytes);
        let planes = [
            PlaneSource {
                bytes: luma,
                bytes_per_row: stride,
                rows: options.height,
            },
            PlaneSource {
                bytes: chroma,
                bytes_per_row: stride,
                rows: options.height.div_ceil(2),
            },
        ];
        let source = SourceFrame {
            frame_id: i64::try_from(sent)? + 1000,
            presentation_time: time(i64::try_from(sent)? * ticks, options.timescale),
            duration: time(ticks, options.timescale),
            force_key_frame: options.force.contains(&sent),
            pixel_format: options.format,
            width: options.width,
            height: options.height,
            planes: &planes,
        };
        let frames = encoder.encode(&source)?;
        gather(frames, encoder.configuration());
        sent += 1;
    }
    let frames = encoder.flush()?;
    gather(frames, encoder.configuration());
    let elapsed = StdRuntime.elapsed_since(start).as_secs_f64();
    drop(encoder);

    let hevc = options.codec == Codec::Hevc;
    let mut log = BufWriter::new(File::create(format!("{}.log", options.prefix))?);
    let mut annex_b = BufWriter::new(File::create(format!(
        "{}.{}",
        options.prefix,
        if hevc { "265" } else { "264" }
    ))?);
    let mut track = Vec::with_capacity(emitted.len());
    let mut written_record = None;
    let mut bytes = 0usize;
    for (index, e) in emitted.iter().enumerate() {
        let f = &e.frame;
        if let Some(error) = &f.error {
            return Err(format!("sample {index}, frame {}: {error}", f.frame_id - 1000).into());
        }
        let (pts, dts) = (
            f.presentation_time.value,
            if f.decode_time.is_valid() {
                f.decode_time.value
            } else {
                f.presentation_time.value
            },
        );
        writeln!(
            log,
            "sample {index} frame {} pts {pts} dts {dts} sync {} depends {} depended {} bytes {} record {}",
            f.frame_id - 1000,
            u8::from(f.sync),
            u8::from(f.depends_on_others),
            u8::from(f.depended_on_by_others),
            f.data.len(),
            e.record
        )?;
        if written_record != Some(e.record) {
            for unit in parameter_sets(hevc, &records[e.record]) {
                annex_b.write_all(&[0, 0, 0, 1])?;
                annex_b.write_all(&unit)?;
            }
            written_record = Some(e.record);
        }
        for unit in length_prefixed_units(&f.data)
            .ok_or_else(|| format!("sample {index}: lengths run past the end"))?
        {
            annex_b.write_all(&[0, 0, 0, 1])?;
            annex_b.write_all(unit)?;
        }
        bytes += f.data.len();
        track.push(mp4::Sample {
            data: f.data.clone(),
            pts,
            dts,
            duration: f.duration.value,
            sync: f.sync,
        });
    }
    let first = records.first().ok_or("no configuration record")?;
    let file = if hevc {
        mp4::write(
            b"hvc1",
            b"hvcC",
            first,
            options.width,
            options.height,
            u32::try_from(options.timescale)?,
            &track,
        )
    } else {
        mp4::write(
            b"avc1",
            b"avcC",
            first,
            options.width,
            options.height,
            u32::try_from(options.timescale)?,
            &track,
        )
    };
    std::fs::write(format!("{}.mp4", options.prefix), file)?;
    let seconds = f64::from(u32::try_from(sent)?) / f64::from(options.fps.max(1));
    eprintln!(
        "{sent} frames, {} samples, {} records, {:.0} bit/s, {:.1} fps",
        emitted.len(),
        records.len(),
        (bytes * 8) as f64 / seconds.max(1e-9),
        f64::from(u32::try_from(sent)?) / elapsed.max(1e-9)
    );
    if u64::try_from(emitted.len())? != sent {
        return Err(format!("{sent} frames gave {} samples", emitted.len()).into());
    }
    Ok(())
}

/// The parameter sets of an `avcC` or `hvcC` record.
fn parameter_sets(hevc: bool, record: &[u8]) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    let take = |at: usize| -> Option<(Vec<u8>, usize)> {
        let length = usize::from(u16::from_be_bytes([*record.get(at)?, *record.get(at + 1)?]));
        Some((
            record.get(at + 2..at + 2 + length)?.to_vec(),
            at + 2 + length,
        ))
    };
    if hevc {
        // hvcC: 22 bytes, numOfArrays, then each array's type, count and
        // units.
        let Some(&arrays) = record.get(22) else {
            return units;
        };
        let mut at = 23;
        for _ in 0..arrays {
            let Some(count) = record
                .get(at + 1..at + 3)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
            else {
                return units;
            };
            at += 3;
            for _ in 0..count {
                let Some((unit, after)) = take(at) else {
                    return units;
                };
                units.push(unit);
                at = after;
            }
        }
    } else {
        // avcC: 5 bytes, the SPS count, the SPSes, the PPS count, the PPSes.
        let mut at = 5;
        for mask in [0x1f, 0xff] {
            let Some(&count) = record.get(at) else {
                return units;
            };
            at += 1;
            for _ in 0..(count & mask) {
                let Some((unit, after)) = take(at) else {
                    return units;
                };
                units.push(unit);
                at = after;
            }
        }
    }
    units
}
