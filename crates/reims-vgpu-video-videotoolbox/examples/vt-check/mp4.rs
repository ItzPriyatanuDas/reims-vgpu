//! Just enough of the ISO base media file format (ISO/IEC 14496-12) to
//! take a video track's decoder configuration record and its samples, with
//! their times, out of an MP4, as AVFoundation hands them to VideoToolbox:
//! the sample entry's codec and size, the record from its extension box,
//! and each sample's bytes, decode time (`stts`) and composition offset
//! (`ctts`), in decode order.

/// One video track.
pub(crate) struct Track {
    /// The sample entry's type: `avc1`, `hvc1`, `av01` and so on.
    pub(crate) codec: [u8; 4],
    /// The decoder configuration record: `avcC`, `hvcC`, `av1C` or `vpcC`.
    pub(crate) record: Vec<u8>,
    /// The sample entry's width and height.
    pub(crate) size: (u32, u32),
    /// The media timescale (`mdhd`).
    pub(crate) timescale: u32,
    /// Each sample, in decode order.
    pub(crate) samples: Vec<Sample>,
}

/// One sample: where it is in the file, and its times in the timescale.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub(crate) offset: usize,
    pub(crate) size: usize,
    pub(crate) decode_time: i64,
    pub(crate) presentation_time: i64,
    pub(crate) duration: i64,
}

fn boxes(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let size = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
        let (header, size) = match size {
            0 => (8, data.len()),
            1 => {
                let large = u64::from_be_bytes(data.get(8..16)?.try_into().ok()?);
                (16, usize::try_from(large).ok()?)
            }
            n => (8, n),
        };
        if size < header || size > data.len() {
            return None;
        }
        let body = &data[header..size];
        data = &data[size..];
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn be16(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes(
        data.get(at..at + 2)?.try_into().ok()?,
    )))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// The record box each sample entry carries.
fn record_box(codec: &[u8; 4]) -> Option<&'static [u8; 4]> {
    Some(match codec {
        b"avc1" | b"avc3" => b"avcC",
        b"hvc1" | b"hev1" => b"hvcC",
        b"av01" => b"av1C",
        b"vp09" => b"vpcC",
        _ => return None,
    })
}

/// The first video track of `file` with a sample entry of a known codec.
pub(crate) fn video_track(file: &[u8]) -> Result<Track, String> {
    let moov = child(file, b"moov").ok_or("no moov box")?;
    for (kind, trak) in boxes(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(mdia) = child(trak, b"mdia") else {
            continue;
        };
        let Some(stbl) = child(mdia, b"minf").and_then(|m| child(m, b"stbl")) else {
            continue;
        };
        let Some((codec, entry)) = child(stbl, b"stsd")
            .and_then(|stsd| stsd.get(8..))
            .and_then(|entries| boxes(entries).next())
        else {
            continue;
        };
        let Some(record) = record_box(&codec) else {
            continue;
        };
        // VisualSampleEntry's fixed fields are 78 bytes, its width and
        // height at 24 and 26; boxes follow.
        let record = entry
            .get(78..)
            .and_then(|boxes| child(boxes, record))
            .ok_or_else(|| format!("no {} box", String::from_utf8_lossy(record)))?;
        // vpcC is a FullBox: the record VideoToolbox takes is the whole
        // box body, version and flags included, as MP4 stores it.
        let size = (be16(entry, 24).unwrap_or(0), be16(entry, 26).unwrap_or(0));
        let mdhd = child(mdia, b"mdhd").ok_or("no mdhd box")?;
        let timescale = if mdhd.first() == Some(&1) {
            be32(mdhd, 20)
        } else {
            be32(mdhd, 12)
        }
        .ok_or("a short mdhd box")?;
        let samples = samples(stbl).ok_or("a sample table that does not read")?;
        return Ok(Track {
            codec,
            record: record.to_vec(),
            size,
            timescale,
            samples,
        });
    }
    Err("no video track of a known codec".into())
}

fn samples(stbl: &[u8]) -> Option<Vec<Sample>> {
    let stsz = child(stbl, b"stsz")?;
    let uniform = be32(stsz, 4)?;
    let count = be32(stsz, 8)? as usize;
    let sizes: Vec<usize> = (0..count)
        .map(|i| {
            if uniform != 0 {
                Some(uniform as usize)
            } else {
                be32(stsz, 12 + 4 * i).map(|s| s as usize)
            }
        })
        .collect::<Option<_>>()?;
    let chunks: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        (0..be32(stco, 4)? as usize)
            .map(|i| be32(stco, 8 + 4 * i).map(u64::from))
            .collect::<Option<_>>()?
    } else {
        let co64 = child(stbl, b"co64")?;
        (0..be32(co64, 4)? as usize)
            .map(|i| be64(co64, 8 + 8 * i))
            .collect::<Option<_>>()?
    };
    let stsc = child(stbl, b"stsc")?;
    let runs: Vec<(usize, usize)> = (0..be32(stsc, 4)? as usize)
        .map(|i| {
            Some((
                be32(stsc, 8 + 12 * i)? as usize,
                be32(stsc, 12 + 12 * i)? as usize,
            ))
        })
        .collect::<Option<_>>()?;
    let mut offsets = Vec::with_capacity(count);
    for (index, &offset) in chunks.iter().enumerate() {
        let per_chunk = runs.iter().rev().find(|&&(first, _)| first <= index + 1)?.1;
        let mut at = usize::try_from(offset).ok()?;
        for _ in 0..per_chunk {
            let size = *sizes.get(offsets.len())?;
            offsets.push((at, size));
            at += size;
        }
    }
    if offsets.len() != count {
        return None;
    }
    // stts: runs of (count, delta); ctts: runs of (count, offset), the
    // offset signed in version 1 and read so in version 0 too, as writers
    // that put negative offsets in version 0 mean them.
    let stts = child(stbl, b"stts")?;
    let mut durations = Vec::with_capacity(count);
    for i in 0..be32(stts, 4)? as usize {
        let (n, delta) = (be32(stts, 8 + 8 * i)?, be32(stts, 12 + 8 * i)?);
        durations.extend(std::iter::repeat_n(i64::from(delta), n as usize));
    }
    let mut offsets_in_time = vec![0i64; count];
    if let Some(ctts) = child(stbl, b"ctts") {
        let mut at = 0usize;
        for i in 0..be32(ctts, 4)? as usize {
            let (n, offset) = (be32(ctts, 8 + 8 * i)?, be32(ctts, 12 + 8 * i)?);
            for _ in 0..n {
                *offsets_in_time.get_mut(at)? = i64::from(offset.cast_signed());
                at += 1;
            }
        }
    }
    let mut decode_time = 0i64;
    let mut samples = Vec::with_capacity(count);
    for (index, (offset, size)) in offsets.into_iter().enumerate() {
        let duration = *durations.get(index).or(durations.last())?;
        samples.push(Sample {
            offset,
            size,
            decode_time,
            presentation_time: decode_time + offsets_in_time[index],
            duration,
        });
        decode_time += duration;
    }
    Some(samples)
}
