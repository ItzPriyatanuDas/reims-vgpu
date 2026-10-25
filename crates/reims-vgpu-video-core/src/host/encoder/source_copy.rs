//! The source frame's picture, copied out of the guest's surface before the
//! backend encodes it, and where a session's frames spend their time (both
//! ours).
//!
//! The backend encodes with no lend of a guest surface held, for host
//! safety: a host may lend one surface at a time in the process, as
//! reims-vgpu does, so a backend call that never returned inside a lend
//! would hold every other lend in the process, the other sessions' copies
//! and the JPEG engine's reads on the device's thread among them. Apple's
//! host hands its encoder the guest's pixel buffer itself, so this copy is
//! a difference of ours, which the guest does not see: the frame's surface
//! is held until the frame is encoded and emitted, as before. The motion
//! session copies its frames out the same way.
//!
//! The copy is one pass over the stream's picture, each plane's rows
//! packed, into a buffer the session keeps: whatever the guest's rows, it
//! is no larger than the picture, and once the first frame has grown it,
//! no frame allocates.

use std::time::Duration;

use crate::backend::PixelFormat;
use crate::surfaces::PlaneSource;

/// A session's copy of its latest source frame's picture.
#[derive(Debug, Default)]
pub(super) struct SourceCopy {
    bytes: Vec<u8>,
    /// Each plane's bytes a row and rows: luma, then interleaved chroma.
    geometry: [(usize, u32); 2],
}

impl SourceCopy {
    /// Copies the top left `width` by `height` of `planes`, luma then
    /// interleaved chroma in `format`: luma's `height` rows of `width`
    /// samples, and chroma's half as many rows, rounded up, of a Cb and Cr
    /// pair for every two columns. False, and nothing copied, where the
    /// planes do not hold that much.
    pub(super) fn copy(
        &mut self,
        planes: &[PlaneSource<'_>],
        format: PixelFormat,
        width: u32,
        height: u32,
    ) -> bool {
        self.bytes.clear();
        let copied = self.copy_rows(planes, format, width, height);
        if copied.is_none() {
            self.bytes.clear();
            self.geometry = [(0, 0); 2];
        }
        copied.is_some()
    }

    fn copy_rows(
        &mut self,
        planes: &[PlaneSource<'_>],
        format: PixelFormat,
        width: u32,
        height: u32,
    ) -> Option<()> {
        let [luma, chroma, ..] = planes else {
            return None;
        };
        let sample = format.bytes_per_sample();
        let width = usize::try_from(width).ok()?;
        let luma_row = width.checked_mul(sample)?;
        let chroma_row = width.div_ceil(2).checked_mul(2)?.checked_mul(sample)?;
        let chroma_rows = height.div_ceil(2);
        let rows = |rows: u32| usize::try_from(rows).ok();
        let total = luma_row
            .checked_mul(rows(height)?)?
            .checked_add(chroma_row.checked_mul(rows(chroma_rows)?)?)?;
        self.bytes.reserve(total);
        for (plane, row, count) in [(luma, luma_row, height), (chroma, chroma_row, chroma_rows)] {
            if plane.bytes_per_row == row {
                // Rows already packed: the plane in one piece.
                let length = row.checked_mul(rows(count)?)?;
                self.bytes.extend_from_slice(plane.bytes.get(..length)?);
                continue;
            }
            for index in 0..rows(count)? {
                let start = index.checked_mul(plane.bytes_per_row)?;
                self.bytes
                    .extend_from_slice(plane.bytes.get(start..start.checked_add(row)?)?);
            }
        }
        self.geometry = [(luma_row, height), (chroma_row, chroma_rows)];
        Some(())
    }

    /// The copy's planes, as [`crate::encoder_backend::SourceFrame::planes`]
    /// gives them to a backend.
    pub(super) fn planes(&self) -> [PlaneSource<'_>; 2] {
        let [(luma_row, luma_rows), (chroma_row, chroma_rows)] = self.geometry;
        let luma_length = usize::try_from(luma_rows)
            .ok()
            .and_then(|rows| rows.checked_mul(luma_row))
            .unwrap_or(0)
            .min(self.bytes.len());
        let (luma, chroma) = self.bytes.split_at(luma_length);
        [
            PlaneSource {
                bytes: luma,
                bytes_per_row: luma_row,
                rows: luma_rows,
            },
            PlaneSource {
                bytes: chroma,
                bytes_per_row: chroma_row,
                rows: chroma_rows,
            },
        ]
    }
}

/// The sums over a session's frames that reached the backend, logged when
/// the session ends: each frame's copy out of its surface, in its lend,
/// and the backend's encode call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct EncodeTimes {
    /// The frames copied and handed to the backend.
    pub(super) frames: u64,
    /// Their copies out of the guest's surfaces.
    pub(super) copy: Duration,
    /// The backend's encode calls.
    pub(super) encode: Duration,
}

impl EncodeTimes {
    /// Adds `took` to the part `part` picks.
    pub(super) fn add(&mut self, part: fn(&mut Self) -> &mut Duration, took: Duration) {
        *part(self) += took;
    }

    /// The log's line, each part's mean a frame; `None` with no frame.
    pub(super) fn summary(&self) -> Option<String> {
        if self.frames == 0 {
            return None;
        }
        let per_frame = |total: Duration| {
            total.as_secs_f64() * 1e3 / f64::from(u32::try_from(self.frames).unwrap_or(u32::MAX))
        };
        Some(format!(
            "{} frames; a frame took {:.3} ms to copy out of its surface and {:.3} ms to encode",
            self.frames,
            per_frame(self.copy),
            per_frame(self.encode),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(bytes: &[u8], bytes_per_row: usize, rows: u32) -> PlaneSource<'_> {
        PlaneSource {
            bytes,
            bytes_per_row,
            rows,
        }
    }

    #[test]
    fn the_picture_is_copied_with_its_rows_packed() {
        // A 3x3 picture in rows of 5, chroma rows of 6: the copy is the
        // picture's 3x3 luma and 2 rows of 2 pairs, at the even width.
        let luma: Vec<u8> = (0..15).collect();
        let chroma: Vec<u8> = (100..112).collect();
        let mut copy = SourceCopy::default();
        assert!(copy.copy(
            &[plane(&luma, 5, 3), plane(&chroma, 6, 2)],
            PixelFormat::Nv12VideoRange,
            3,
            3
        ));
        let [y, c] = copy.planes();
        assert_eq!(
            (y.bytes, y.bytes_per_row, y.rows),
            (&[0, 1, 2, 5, 6, 7, 10, 11, 12][..], 3, 3)
        );
        assert_eq!(
            (c.bytes, c.bytes_per_row, c.rows),
            (&[100, 101, 102, 103, 106, 107, 108, 109][..], 4, 2)
        );
        // Packed rows go in one piece, and 10-bit samples are two bytes.
        let luma: Vec<u8> = (0..8).collect();
        let chroma: Vec<u8> = (50..54).collect();
        assert!(copy.copy(
            &[plane(&luma, 4, 2), plane(&chroma, 4, 1)],
            PixelFormat::P010VideoRange,
            2,
            2
        ));
        let [y, c] = copy.planes();
        assert_eq!(y.bytes, &luma[..]);
        assert_eq!(c.bytes, &chroma[..]);
    }

    #[test]
    fn planes_that_do_not_hold_the_picture_copy_nothing() {
        let short = [0u8; 8];
        let mut copy = SourceCopy::default();
        assert!(!copy.copy(
            &[plane(&short, 4, 3), plane(&short, 4, 2)],
            PixelFormat::Nv12VideoRange,
            4,
            3
        ));
        assert!(!copy.copy(&[plane(&short, 4, 2)], PixelFormat::Nv12VideoRange, 4, 2));
        let [y, c] = copy.planes();
        assert!(y.bytes.is_empty() && c.bytes.is_empty());
    }

    #[test]
    fn the_summary_gives_each_parts_mean() {
        assert_eq!(EncodeTimes::default().summary(), None);
        let times = EncodeTimes {
            frames: 4,
            copy: Duration::from_millis(2),
            encode: Duration::from_millis(40),
        };
        assert_eq!(
            times.summary().as_deref(),
            Some(
                "4 frames; a frame took 0.500 ms to copy out of its surface and 10.000 ms to encode"
            )
        );
    }
}
