//! Planar 4:2:0 frames, as a software decoder writes them, packed as the
//! guest's buffers hold them, for the suite's NV12 and P010 references.
//!
//! The planar layout is ffmpeg's `yuv420p` and `yuv420p10le`: the luma,
//! then Cb, then Cr, each plane's rows packed, the chroma planes
//! `width.div_ceil(2)` by `height.div_ceil(2)`, a 10-bit sample in the low
//! bits of a little-endian 16-bit word. The packed layout is the one the
//! stand-in guest hashes ([`crate::decode`]): the luma, then a row of Cb
//! and Cr pairs for every two luma rows, a pair for every two luma samples,
//! the last one too at an odd width; NV12 at 8 bits, and P010 at 10, each
//! sample's ten bits at the top of a little-endian 16-bit word.
//!
//! So a reference in NV12 or P010 is a software decode's own samples, with
//! no conversion of ffmpeg's: its P010 of an odd width gets the last Cb/Cr
//! pair wrong (docs/video/vaapi.md, "AV1 on the AMD GPUs").

/// The bits of a frame's samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// 8 bits, a byte a sample: `yuv420p` packed into NV12.
    Eight,
    /// 10 bits, two bytes a sample: `yuv420p10le` packed into P010.
    Ten,
}

impl Depth {
    /// The depth of `bits`, 8 or 10.
    pub fn of_bits(bits: u32) -> Option<Self> {
        match bits {
            8 => Some(Self::Eight),
            10 => Some(Self::Ten),
            _ => None,
        }
    }

    /// The bytes of one sample.
    pub const fn bytes(self) -> usize {
        match self {
            Self::Eight => 1,
            Self::Ten => 2,
        }
    }
}

/// The bytes of a planar 4:2:0 frame of `(width, height)` at `depth`, both
/// layouts' length, or None where that overflows.
pub fn frame_len((width, height): (u32, u32), depth: Depth) -> Option<usize> {
    let (width, height) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    let luma = width.checked_mul(height)?;
    let chroma = width.div_ceil(2).checked_mul(height.div_ceil(2))?;
    luma.checked_add(chroma.checked_mul(2)?)?
        .checked_mul(depth.bytes())
}

/// `frame`, planar, packed into NV12 or P010 at `(width, height)`. An
/// error is a frame of another length than [`frame_len`]'s, or a 10-bit
/// sample past 1023.
pub fn pack(frame: &[u8], size: (u32, u32), depth: Depth) -> Result<Vec<u8>, String> {
    let len = frame_len(size, depth).ok_or("the frame's size overflows")?;
    if frame.len() != len {
        return Err(format!(
            "{} bytes, where a {}x{} frame of {} bits has {len}",
            frame.len(),
            size.0,
            size.1,
            if depth == Depth::Eight { 8 } else { 10 },
        ));
    }
    let sample = depth.bytes();
    let luma = usize::try_from(size.0).unwrap_or(0) * usize::try_from(size.1).unwrap_or(0) * sample;
    let (luma, chroma) = frame.split_at(luma);
    let (cb, cr) = chroma.split_at(chroma.len() / 2);
    let mut packed = Vec::with_capacity(len);
    match depth {
        Depth::Eight => {
            packed.extend_from_slice(luma);
            for (&cb, &cr) in cb.iter().zip(cr) {
                packed.extend_from_slice(&[cb, cr]);
            }
        }
        Depth::Ten => {
            for &luma in luma.as_chunks::<2>().0 {
                packed.extend_from_slice(&p010(luma)?);
            }
            for (&cb, &cr) in cb.as_chunks::<2>().0.iter().zip(cr.as_chunks::<2>().0) {
                packed.extend_from_slice(&p010(cb)?);
                packed.extend_from_slice(&p010(cr)?);
            }
        }
    }
    Ok(packed)
}

/// A `yuv420p10le` sample's two bytes as P010's: its ten bits at the top.
fn p010(sample: [u8; 2]) -> Result<[u8; 2], String> {
    let value = u16::from_le_bytes(sample);
    if value > 0x3ff {
        return Err(format!("a sample of {value:#x}, past 10 bits"));
    }
    Ok((value << 6).to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::{Depth, frame_len, pack};

    /// An 8-bit planar frame of `(width, height)` whose samples count up
    /// in each plane: from 0 in the luma, 100 in Cb and 200 in Cr.
    fn counting(width: usize, height: usize) -> Vec<u8> {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let plane = |n: usize, base: usize| (0..n).map(move |i| ((base + i) % 251) as u8);
        plane(width * height, 0)
            .chain(plane(cw * ch, 100))
            .chain(plane(cw * ch, 200))
            .collect()
    }

    #[test]
    fn nv12_interleaves_every_chroma_pair_at_an_odd_width() -> Result<(), String> {
        // 3x3: luma 9, chroma 2x2 a plane.
        let frame = counting(3, 3);
        let packed = pack(&frame, (3, 3), Depth::Eight)?;
        assert_eq!(&packed[..9], &frame[..9]);
        // Rows of two pairs: the second pair is the odd column's.
        assert_eq!(&packed[9..], &[100, 200, 101, 201, 102, 202, 103, 203]);
        Ok(())
    }

    #[test]
    fn p010_puts_ten_bits_at_the_top_and_keeps_the_last_pair() -> Result<(), String> {
        // 3x1 at 10 bits: luma 3 samples, chroma 2x1 a plane.
        let samples: [u16; 7] = [0, 1, 0x3ff, 0x10, 0x11, 0x20, 0x21];
        let frame: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let packed = pack(&frame, (3, 1), Depth::Ten)?;
        let words: Vec<u16> = (packed.as_chunks::<2>().0.iter())
            .map(|&w| u16::from_le_bytes(w))
            .collect();
        assert_eq!(
            words,
            [
                0,
                1 << 6,
                0x3ff << 6,
                0x10 << 6,
                0x20 << 6,
                0x11 << 6,
                0x21 << 6
            ]
        );
        Ok(())
    }

    #[test]
    fn a_frame_of_another_length_or_past_ten_bits_is_refused() {
        let frame = counting(4, 4);
        assert!(pack(&frame[1..], (4, 4), Depth::Eight).is_err());
        assert!(pack(&frame, (4, 2), Depth::Eight).is_err());
        let mut ten = vec![0u8; frame_len((2, 2), Depth::Ten).unwrap_or(0)];
        assert!(pack(&ten, (2, 2), Depth::Ten).is_ok());
        ten[1] = 0x04;
        assert!(pack(&ten, (2, 2), Depth::Ten).is_err());
    }

    #[test]
    fn the_lengths_count_the_odd_sides_chroma() {
        assert_eq!(
            frame_len((641, 361), Depth::Eight),
            Some(641 * 361 + 2 * 321 * 181)
        );
        assert_eq!(
            frame_len((643, 363), Depth::Ten),
            Some(2 * (643 * 363 + 2 * 322 * 182))
        );
        assert_eq!(frame_len((u32::MAX, u32::MAX), Depth::Ten), None);
    }
}
