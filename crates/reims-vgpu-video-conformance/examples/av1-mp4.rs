//! Writes an AV1 IVF file's temporal units as an MP4, each a sample, for a
//! stream ffmpeg's `-c copy` will not remux. Ours.
//!
//! ```sh
//! av1-mp4 IN.ivf OUT.mp4
//! ```
//!
//! ffmpeg flags an AV1 packet a key frame only where the frame it shows is
//! one, and its stream copy drops every packet before the first such: a
//! stream whose every key frame is hidden, shown through an inter frame
//! instead (libaom's `--enable-keyframe-filtering=2`), came out with no
//! sample at all (ffmpeg n9.0.2, 2026-10-24). Here the `av1C` record
//! is made from the first sequence header OBU (AV1-ISOBMFF 2.3), the
//! sample entry's size is the IVF header's, the samples are 1/30 s apart,
//! and the first is a sync sample.

use reims_vgpu_video_codec::av1::obu::{ObuType, obus};
use reims_vgpu_video_codec::av1::sequence::SequenceHeader;
use reims_vgpu_video_conformance::{ivf, mp4_write};

/// The `av1C` record for the sequence header OBU `obu` (its whole bytes,
/// header and size included) whose payload parsed as `s`.
fn av1c(s: &SequenceHeader, obu: &[u8]) -> Vec<u8> {
    let point = s.operating_point();
    let c = &s.color;
    let bits = |b: bool, at: u8| u8::from(b) << at;
    let mut record = vec![
        // marker 1, version 1.
        0x81,
        (s.seq_profile << 5) | (point.seq_level_idx & 0x1f),
        bits(point.seq_tier, 7)
            | bits(c.bit_depth > 8, 6)
            | bits(c.bit_depth == 12, 5)
            | bits(c.mono_chrome, 4)
            | bits(c.subsampling_x, 3)
            | bits(c.subsampling_y, 2)
            | (c.chroma_sample_position & 3),
        // No initial presentation delay.
        0,
    ];
    record.extend_from_slice(obu);
    record
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] =
        <[String; 2]>::try_from(args).map_err(|_| "usage: av1-mp4 IN.ivf OUT.mp4".to_owned())?;
    let file = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let frames = ivf::frames(&file)?;
    let size = |at: usize| {
        file.get(at..at + 2)
            .map_or(0, |b| u32::from(u16::from_le_bytes([b[0], b[1]])))
    };
    let (width, height) = (size(12), size(14));
    let mut record = None;
    let mut samples = Vec::with_capacity(frames.len());
    for (i, &(offset, length)) in frames.iter().enumerate() {
        let data = file
            .get(offset..offset + length)
            .ok_or("a temporal unit past the file's end")?;
        if record.is_none() {
            let mut at = 0;
            for obu in obus(data) {
                let obu = obu.map_err(|e| format!("temporal unit {i}: {e}"))?;
                // The OBU's whole bytes: its payload ends the slice taken.
                let end =
                    obu.payload.as_ptr() as usize - data.as_ptr() as usize + obu.payload.len();
                if obu.obu_type == ObuType::SequenceHeader {
                    let s = SequenceHeader::parse(obu.payload)
                        .map_err(|e| format!("temporal unit {i}: sequence header: {e}"))?;
                    record = Some(av1c(&s, &data[at..end]));
                    break;
                }
                at = end;
            }
        }
        let time = i64::try_from(i).map_err(|_| "too many temporal units")?;
        samples.push(mp4_write::Sample {
            data: data.to_vec(),
            pts: time,
            dts: time,
            duration: 1,
            sync: i == 0,
        });
    }
    let record = record.ok_or("no sequence header OBU")?;
    let out = mp4_write::write(b"av01", b"av1C", &record, width, height, 30, &samples);
    std::fs::write(&output, out).map_err(|e| format!("{output}: {e}"))?;
    println!("{output}: {} samples, {width}x{height}", samples.len());
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("av1-mp4: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
