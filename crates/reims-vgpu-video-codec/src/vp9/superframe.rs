//! Superframes (Annex B): several frames in one chunk, as an MP4 sample or
//! an IVF frame carries a hidden frame with the frame shown after it. An
//! index at the chunk's end gives each frame's size; a chunk without one
//! is a single frame.

use super::Error;
use crate::num::ToUsize;

/// The top three bits of an index's first and last bytes
/// (`superframe_marker`, B.2).
const SUPERFRAME_MARKER: u8 = 0b110;

/// The frames of `chunk`, in order. A chunk that ends in a superframe
/// index is split by it; the index itself is not part of any frame.
/// Anything else is one frame.
///
/// An index whose sizes run past the chunk is refused. Bytes between the
/// last frame and the index are dropped, as the decoding process ignores
/// them (B.3).
pub fn frames(chunk: &[u8]) -> Result<Vec<&[u8]>, Error> {
    let Some(sizes) = index(chunk)? else {
        return Ok(if chunk.is_empty() {
            Vec::new()
        } else {
            vec![chunk]
        });
    };
    let mut frames = Vec::with_capacity(sizes.len());
    let mut rest = chunk;
    for size in sizes {
        let (frame, after) = rest
            .split_at_checked(size)
            .ok_or(Error::SuperframeIndexPastData)?;
        // A zero size is no frame (libvpx's encoder never writes one, and
        // its decoder skips it).
        if !frame.is_empty() {
            frames.push(frame);
        }
        rest = after;
    }
    Ok(frames)
}

/// The frame sizes of `chunk`'s superframe index, if it ends in one: its
/// last byte is a marker byte, and the byte the marker says the index
/// starts at is the same marker.
fn index(chunk: &[u8]) -> Result<Option<Vec<usize>>, Error> {
    let Some((&marker, before_marker)) = chunk.split_last() else {
        return Ok(None);
    };
    if marker >> 5 != SUPERFRAME_MARKER {
        return Ok(None);
    }
    // frames_in_superframe_minus_1 and bytes_per_framesize_minus_1 (B.2),
    // each of a few bits.
    let count = usize::from(marker & 0x7) + 1;
    let size_bytes = usize::from((marker >> 3) & 0x3) + 1;
    // The index is the marker, the sizes, then the marker again.
    let sizes_and_marker = size_bytes
        .checked_mul(count)
        .and_then(|sizes| sizes.checked_add(1))
        .ok_or(Error::Overflow("the superframe index's size"))?;
    let Some((frames, index)) = before_marker
        .len()
        .checked_sub(sizes_and_marker)
        .and_then(|start| before_marker.split_at_checked(start))
    else {
        return Ok(None);
    };
    let Some((&first, sizes)) = index.split_first() else {
        return Ok(None);
    };
    if first != marker {
        return Ok(None);
    }
    // frame_sizes, little-endian.
    let sizes: Vec<usize> = sizes
        .chunks_exact(size_bytes)
        .map(|bytes| {
            let mut size = [0u8; 4];
            for (to, &from) in size.iter_mut().zip(bytes) {
                *to = from;
            }
            u32::from_le_bytes(size).to_usize()
        })
        .collect();
    let total = sizes
        .iter()
        .try_fold(0usize, |total, &size| total.checked_add(size));
    if total.is_none_or(|total| total > frames.len()) {
        return Err(Error::SuperframeIndexPastData);
    }
    Ok(Some(sizes))
}
