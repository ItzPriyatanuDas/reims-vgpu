//! Writes an AV1 MP4 whose one sample is padded to a size asked for, for
//! testing how the guest's transport carries a large sample. Ours.
//!
//! ```sh
//! av1-pad IN.mp4 OUT.mp4 SAMPLE SIZE
//! ```
//!
//! Sample `SAMPLE` (0-based, in decode order) gets an `OBU_PADDING` at its
//! end that makes it `SIZE` bytes. A decoder ignores padding OBUs (AV1
//! 5.3.1, 6.2.1, `obu_type` 15), so the stream decodes to the frames IN
//! does, and IN's reference MD5s hold for OUT. The other samples, the
//! `av1C` record and the sample entry's size are IN's; the samples are
//! 1/30 s apart, the first a sync sample.

use reims_vgpu_video_conformance::{mp4, mp4_write};

/// `OBU_PADDING` with `obu_has_size_field` set: `obu_type` 15 in bits 6 to
/// 3, the size flag in bit 1 (AV1 5.3.2).
const PADDING_HEADER: u8 = (15 << 3) | (1 << 1);

/// `value` as `leb128()` (AV1 4.10.5).
fn leb128(mut value: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            bytes.push(byte);
            return bytes;
        }
        bytes.push(byte | 0x80);
    }
}

/// A padding OBU of exactly `total` bytes, header and size field
/// included, or None when no padding OBU is that long.
fn padding_obu(total: usize) -> Option<Vec<u8>> {
    (0..total).rev().find_map(|payload| {
        let size = leb128(payload);
        (1 + size.len() + payload == total).then(|| {
            let mut obu = vec![PADDING_HEADER];
            obu.extend_from_slice(&size);
            obu.resize(total, 0);
            obu
        })
    })
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output, sample, size] = <[String; 4]>::try_from(args)
        .map_err(|_| "usage: av1-pad IN.mp4 OUT.mp4 SAMPLE SIZE".to_owned())?;
    let index: usize = sample.parse().map_err(|_| "SAMPLE is a number")?;
    let size: usize = size.parse().map_err(|_| "SIZE is a number")?;
    let file = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let track = mp4::av1_track(&file)?;
    let mut samples = Vec::with_capacity(track.samples.len());
    for (i, &(offset, length)) in track.samples.iter().enumerate() {
        let mut data = file
            .get(offset..offset + length)
            .ok_or("a sample past the file's end")?
            .to_vec();
        if i == index {
            let more = size
                .checked_sub(data.len())
                .filter(|&n| n > 0)
                .ok_or(format!("sample {i} is {} bytes already", data.len()))?;
            data.extend(padding_obu(more).ok_or(format!("no padding OBU of {more} bytes"))?);
        }
        let time = i64::try_from(i).map_err(|_| "too many samples")?;
        samples.push(mp4_write::Sample {
            data,
            pts: time,
            dts: time,
            duration: 1,
            sync: i == 0,
        });
    }
    if index >= samples.len() {
        return Err(format!("the track has {} samples", samples.len()));
    }
    let (width, height) = track.size;
    let out = mp4_write::write(b"av01", b"av1C", &track.avcc, width, height, 30, &samples);
    std::fs::write(&output, out).map_err(|e| format!("{output}: {e}"))?;
    println!(
        "{output}: sample {index} padded to {size} bytes, {} samples",
        samples.len()
    );
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("av1-pad: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_padding_obu_is_the_length_asked_for() {
        for total in [2, 3, 129, 131, 16_386, 2_093_056] {
            let obu = padding_obu(total).unwrap_or_default();
            assert_eq!(obu.len(), total);
            assert_eq!(obu[0], PADDING_HEADER);
        }
        // One byte holds the header and no size field; 130 bytes would
        // be a 128-byte payload with a 2-byte size, or 127 with a 1-byte
        // one.
        assert_eq!(padding_obu(1), None);
        assert_eq!(padding_obu(130), None);
    }
}
