//! What a motion-estimation backend gives the host session's
//! motion-estimation sessions: a field of motion vectors between two
//! frames.
//!
//! The session does everything the guest sees: the processor list
//! (`meps`), the properties, the pixel buffer attributes it asks the guest
//! for (`Mspa`, `Mvpb`), the vector buffer it asks for (`Mbuf`) and the
//! emission (`Memt`). A backend only estimates. It opens an estimator when
//! the guest starts its session (`mest`), and estimates each pair of
//! frames the guest sends (`mepf`) into a [`MotionField`] on whatever grid
//! of blocks it has. Core then puts the field on Apple's grid
//! ([`AppleGrid`], [`resample`]) and writes it into the guest's buffer as
//! two half floats a vector (`2C0h`), so every backend's vectors reach the
//! guest in one shape.

use crate::backend::{BackendError, PixelFormat};
use crate::surfaces::PlaneSource;

/// The smallest side a session starts at: Apple's host answers -12902 for a
/// side of 1 (measured on the M3 Max, macOS 27, 2026-10-24).
pub const MIN_SIDE: u32 = 2;
/// The largest side a session starts at, as Apple's host.
pub const MAX_SIDE: u32 = 16384;
/// The largest area a session starts at: Apple's host started 16384x8192
/// and refused 16384x16384.
pub const MAX_AREA: u64 = 16384 * 8192;

/// The side of a block Apple's grid gives a vector to, in pixels, as the
/// guest names it in `MotionVectorSize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockSize {
    /// One vector per 16x16 block: the default, and what every value but 4
    /// gives on Apple's host (8, 12, 32, 0 and -4 read back 16, measured).
    Sixteen,
    /// One vector per 4x4 block.
    Four,
}

impl BlockSize {
    /// The block size `MotionVectorSize` asks for: 4 is 4, and anything
    /// else 16, as Apple's processor reads it.
    pub fn from_motion_vector_size(size: Option<i64>) -> Self {
        if size == Some(4) {
            Self::Four
        } else {
            Self::Sixteen
        }
    }

    /// Its side in pixels.
    pub const fn pixels(self) -> u32 {
        match self {
            Self::Sixteen => 16,
            Self::Four => 4,
        }
    }
}

/// Apple's grid of vectors for a picture: one per block of
/// [`BlockSize`], over whole 16x16 blocks, at least 10 by 4 of them, so
/// that 16x16 has 10x4 vectors at 16 and 40x16 at 4 (measured in Apple's
/// `Mvpb` on the M3 Max, macOS 27). Blocks wholly outside the picture carry (0, 0), as
/// Apple's did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppleGrid {
    /// The picture's width.
    pub width: u32,
    /// The picture's height.
    pub height: u32,
    /// The blocks' size.
    pub block: BlockSize,
}

impl AppleGrid {
    /// The vectors across.
    pub fn columns(&self) -> u32 {
        self.width.div_ceil(16).max(10) * (16 / self.block.pixels())
    }

    /// The vectors down.
    pub fn rows(&self) -> u32 {
        self.height.div_ceil(16).max(4) * (16 / self.block.pixels())
    }
}

/// One vector, in pixels: where the current frame's block is found in the
/// reference, relative to the block. Content that moved by (+8, +4) from
/// the reference to the current frame is (-8, -4), as Apple's processor
/// gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MotionVector {
    /// Across, positive to the right.
    pub x: f32,
    /// Down, positive downward.
    pub y: f32,
}

/// What an estimator found: one vector per block of its own grid.
#[derive(Clone, Debug, PartialEq)]
pub struct MotionField {
    /// The side of each block, in pixels.
    pub block_size: u32,
    /// The blocks across.
    pub columns: u32,
    /// The blocks down.
    pub rows: u32,
    /// The vectors, row by row: `columns * rows` of them.
    pub vectors: Vec<MotionVector>,
}

impl MotionField {
    /// The vector of the block at `column`, `row`, clamped to the field,
    /// or (0, 0) when the field has none.
    pub fn at(&self, column: u32, row: u32) -> MotionVector {
        if self.columns == 0 || self.rows == 0 {
            return MotionVector::default();
        }
        let column = column.min(self.columns - 1);
        let row = row.min(self.rows - 1);
        usize::try_from(u64::from(row) * u64::from(self.columns) + u64::from(column))
            .ok()
            .and_then(|index| self.vectors.get(index))
            .copied()
            .unwrap_or_default()
    }
}

/// What a backend estimates on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MotionEstimationCapabilities {
    /// The source pixel formats it takes, in the order the host asks the
    /// guest for them (`Mspa`): the guest converts a client's frames to the
    /// first.
    pub source_formats: Vec<PixelFormat>,
    /// The widest picture it takes, at most [`MAX_SIDE`].
    pub max_width: u32,
    /// The tallest picture it takes, at most [`MAX_SIDE`].
    pub max_height: u32,
}

/// What an estimator is opened with, from the guest's `+mep` and `mest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MotionEstimatorConfig {
    /// The pictures' width.
    pub width: u32,
    /// The pictures' height.
    pub height: u32,
    /// The block size the guest asked for. A backend may estimate on
    /// blocks of its own: core puts its field on Apple's grid.
    pub block: BlockSize,
    /// Whether the guest asked for multipass search, or true motion. A
    /// backend may search harder, or ignore it.
    pub true_motion: bool,
}

/// One frame of a pair: its format, and its planes as the guest lent them.
#[derive(Clone, Copy, Debug)]
pub struct MotionPicture<'a> {
    /// Its pixel format, one of the backend's [`MotionEstimationCapabilities::source_formats`].
    pub pixel_format: PixelFormat,
    /// Its planes: luma, then interleaved chroma, each with its own bytes
    /// per row and at least the picture's rows.
    pub planes: &'a [PlaneSource<'a>],
}

/// A motion-estimation backend: what it takes, and estimators.
///
/// One backend serves every motion-estimation session, from several
/// threads at once: `capabilities` is asked on the device's thread as the
/// guest opens a session, so it answers with no driver call that may
/// stall, and `open` runs on the session's own thread. Neither is called
/// inside a lend of a guest surface.
pub trait MotionEstimationBackend: Send + Sync {
    /// What it estimates on, which the host lists to the guest (`meps`) and
    /// asks the guest's sources to be (`Mspa`).
    fn capabilities(&self) -> MotionEstimationCapabilities;

    /// Opens an estimator for `config`.
    fn open(
        &self,
        config: &MotionEstimatorConfig,
    ) -> Result<Box<dyn MotionEstimator>, BackendError>;
}

/// One open estimator, which its session calls on the session's thread,
/// one call at a time, with no guest surface lent: the two pictures are
/// copies the session made.
pub trait MotionEstimator: Send {
    /// The motion from `reference` to `current`, both of the size the
    /// estimator was opened with.
    fn estimate(
        &mut self,
        reference: &MotionPicture<'_>,
        current: &MotionPicture<'_>,
    ) -> Result<MotionField, BackendError>;
}

/// The most field blocks a grid block takes its vote from, across and
/// down: a field on blocks much smaller than the grid's is sampled evenly,
/// so that a dense field costs no more than this a vector.
const VOTES_PER_SIDE: u64 = 4;

/// Puts `field` on `grid`: each of the grid's blocks that the picture
/// reaches takes the vector most of the field's blocks under its part of
/// the picture carry (up to 4 by 4 of them, evenly spaced), the first of
/// them in a tie; one the picture does not reach is (0, 0). A field on
/// larger blocks than the grid's gives each grid block the vector of the
/// field block under its top left corner.
pub fn resample(field: &MotionField, grid: &AppleGrid) -> Vec<MotionVector> {
    let capacity = usize::try_from(u64::from(grid.columns()) * u64::from(grid.rows())).unwrap_or(0);
    let mut vectors = Vec::with_capacity(capacity);
    let mut row_vectors = Vec::new();
    for row in 0..grid.rows() {
        resample_row(field, grid, row, grid.columns(), &mut row_vectors);
        vectors.extend_from_slice(&row_vectors);
    }
    vectors
}

/// The first `columns` vectors of row `row` of [`resample`], at most the
/// grid's, into `vectors`, which it clears first.
pub fn resample_row(
    field: &MotionField,
    grid: &AppleGrid,
    row: u32,
    columns: u32,
    vectors: &mut Vec<MotionVector>,
) {
    vectors.clear();
    let side = u64::from(grid.block.pixels());
    let block = u64::from(field.block_size.max(1));
    let (width, height) = (u64::from(grid.width), u64::from(grid.height));
    let index = |pixel: u64| u32::try_from(pixel / block).unwrap_or(u32::MAX);
    // The field blocks a span of pixels covers, evenly sampled.
    let samples = |first: u64, last: u64| {
        let (first, last) = (index(first), index(last));
        let span = u64::from(last - first) + 1;
        let step = span.div_ceil(VOTES_PER_SIDE).max(1);
        (0..span)
            .step_by(usize::try_from(step).unwrap_or(1))
            .map(move |offset| first.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX)))
    };
    let mut votes: Vec<(MotionVector, u32)> = Vec::new();
    // In u64, so that no grid's corner overflows.
    let top = u64::from(row) * side;
    for column in 0..columns.min(grid.columns()) {
        let left = u64::from(column) * side;
        if left >= width || top >= height {
            vectors.push(MotionVector::default());
            continue;
        }
        let right = (left + side).min(width) - 1;
        let bottom = (top + side).min(height) - 1;
        votes.clear();
        for field_row in samples(top, bottom) {
            for field_column in samples(left, right) {
                let vector = field.at(field_column, field_row);
                match votes.iter_mut().find(|(seen, _)| *seen == vector) {
                    Some((_, count)) => *count += 1,
                    None => votes.push((vector, 1)),
                }
            }
        }
        let mut best = (MotionVector::default(), 0);
        for &(vector, count) in &votes {
            if count > best.1 {
                best = (vector, count);
            }
        }
        vectors.push(best.0);
    }
}

/// `vector` as `2C0h` stores it: x then y, each an IEEE half float, little
/// endian.
pub fn half_float_pair(vector: MotionVector) -> [u8; 4] {
    let [x0, x1] = half_float(vector.x).to_le_bytes();
    let [y0, y1] = half_float(vector.y).to_le_bytes();
    [x0, x1, y0, y1]
}

/// An `f32` as an IEEE 754 binary16, rounded to nearest, ties to even;
/// past the largest half float it is infinite, and NaN stays NaN.
pub fn half_float(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = u16::try_from((bits >> 16) & 0x8000).unwrap_or(0);
    let exponent = i32::try_from((bits >> 23) & 0xff).unwrap_or(0);
    let mantissa = bits & 0x007f_ffff;
    if exponent == 0xff {
        // Infinity, or a NaN kept quiet.
        return sign | 0x7c00 | if mantissa == 0 { 0 } else { 0x0200 };
    }
    let unbiased = exponent - 127;
    if unbiased > 15 {
        return sign | 0x7c00;
    }
    if unbiased >= -14 {
        // Normal: 10 of the 23 mantissa bits, rounded.
        let half = (u32::try_from(unbiased + 15).unwrap_or(0) << 10) | (mantissa >> 13);
        let rest = mantissa & 0x1fff;
        let rounded = if rest > 0x1000 || (rest == 0x1000 && half & 1 == 1) {
            half + 1
        } else {
            half
        };
        // A carry into the exponent is right, up to infinity.
        return sign | u16::try_from(rounded).unwrap_or(0x7c00);
    }
    if unbiased < -25 {
        return sign;
    }
    // Subnormal: the implicit bit joins the mantissa, shifted down.
    let full = mantissa | 0x0080_0000;
    let shift = u32::try_from(-14 - unbiased + 13).unwrap_or(31);
    let half = full >> shift;
    let rest = full & ((1 << shift) - 1);
    let midpoint = 1 << (shift - 1);
    let rounded = if rest > midpoint || (rest == midpoint && half & 1 == 1) {
        half + 1
    } else {
        half
    };
    sign | u16::try_from(rounded).unwrap_or(0)
}

/// A binary16 as an `f32`, exactly.
pub fn from_half_float(half: u16) -> f32 {
    let sign = if half & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = i32::from((half >> 10) & 0x1f);
    let mantissa = f32::from(half & 0x03ff);
    match exponent {
        0 => sign * mantissa * 2f32.powi(-24),
        0x1f if mantissa == 0.0 => sign * f32::INFINITY,
        0x1f => f32::NAN,
        _ => sign * (1.0 + mantissa / 1024.0) * 2f32.powi(exponent - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(x: f32, y: f32) -> MotionVector {
        MotionVector { x, y }
    }

    #[test]
    fn apples_grid_is_whole_16x16_blocks_at_least_10_by_4() {
        let grid = |width, height, block| AppleGrid {
            width,
            height,
            block,
        };
        let sixteen = BlockSize::Sixteen;
        let four = BlockSize::Four;
        let sides = |g: AppleGrid| (g.columns(), g.rows());
        assert_eq!(sides(grid(1920, 1080, sixteen)), (120, 68));
        assert_eq!(sides(grid(1920, 1080, four)), (480, 272));
        assert_eq!(sides(grid(16, 16, sixteen)), (10, 4));
        assert_eq!(sides(grid(16, 16, four)), (40, 16));
        assert_eq!(sides(grid(256, 128, sixteen)), (16, 8));
        assert_eq!(sides(grid(16384, 8192, four)), (4096, 2048));
    }

    #[test]
    fn every_size_but_4_is_16() {
        assert_eq!(BlockSize::from_motion_vector_size(Some(4)), BlockSize::Four);
        for size in [None, Some(16), Some(8), Some(32), Some(0), Some(-4)] {
            assert_eq!(BlockSize::from_motion_vector_size(size), BlockSize::Sixteen);
        }
    }

    #[test]
    fn a_field_on_smaller_blocks_takes_the_most_common_vector() {
        // 32x16 on 8x8 blocks: four across, two down.
        let a = vector(-8.0, -4.0);
        let b = vector(1.0, 0.0);
        let field = MotionField {
            block_size: 8,
            columns: 4,
            rows: 2,
            vectors: vec![a, a, b, b, b, a, b, a],
        };
        let grid = AppleGrid {
            width: 32,
            height: 16,
            block: BlockSize::Sixteen,
        };
        let vectors = resample(&field, &grid);
        assert_eq!(vectors.len(), 40);
        // The left block's four are a, a, b, a; the right one's b, b, b, a.
        assert_eq!(vectors[0], a);
        assert_eq!(vectors[1], b);
        assert!(vectors[2..].iter().all(|&v| v == MotionVector::default()));
    }

    #[test]
    fn a_tie_goes_to_the_first_and_larger_blocks_are_shared() {
        let a = vector(2.0, 0.0);
        let b = vector(0.0, 2.0);
        let field = MotionField {
            block_size: 8,
            columns: 2,
            rows: 2,
            vectors: vec![a, b, b, a],
        };
        let grid = AppleGrid {
            width: 16,
            height: 16,
            block: BlockSize::Sixteen,
        };
        assert_eq!(resample(&field, &grid)[0], a);
        let coarse = MotionField {
            block_size: 16,
            columns: 1,
            rows: 1,
            vectors: vec![b],
        };
        let fine = AppleGrid {
            width: 16,
            height: 16,
            block: BlockSize::Four,
        };
        let vectors = resample(&coarse, &fine);
        assert_eq!(vectors.len(), 40 * 16);
        for row in 0..16 {
            for column in 0..40 {
                let expected = if row < 4 && column < 4 {
                    b
                } else {
                    MotionVector::default()
                };
                assert_eq!(vectors[row * 40 + column], expected, "{column},{row}");
            }
        }
    }

    #[test]
    fn a_short_field_is_clamped_to_its_edge() {
        let field = MotionField {
            block_size: 16,
            columns: 1,
            rows: 1,
            vectors: vec![vector(-3.0, 2.0)],
        };
        let grid = AppleGrid {
            width: 48,
            height: 16,
            block: BlockSize::Sixteen,
        };
        let vectors = resample(&field, &grid);
        assert_eq!(&vectors[..3], &[vector(-3.0, 2.0); 3]);
        let empty = MotionField {
            block_size: 16,
            columns: 0,
            rows: 0,
            vectors: Vec::new(),
        };
        assert!(
            resample(&empty, &grid)
                .iter()
                .all(|&v| v == MotionVector::default())
        );
    }

    #[test]
    fn half_floats_round_to_nearest_even() {
        assert_eq!(half_float(0.0), 0);
        assert_eq!(half_float(-0.0), 0x8000);
        assert_eq!(half_float(1.0), 0x3c00);
        assert_eq!(half_float(-8.0), 0xc800);
        assert_eq!(half_float(-4.0), 0xc400);
        assert_eq!(half_float(0.25), 0x3400);
        assert_eq!(half_float(65504.0), 0x7bff);
        assert_eq!(half_float(65520.0), 0x7c00);
        assert_eq!(half_float(1e9), 0x7c00);
        assert_eq!(half_float(f32::NEG_INFINITY), 0xfc00);
        assert_eq!(half_float(f32::NAN) & 0x7e00, 0x7e00);
        // 2^-24, the smallest subnormal, and half of it, which ties to 0.
        assert_eq!(half_float(2f32.powi(-24)), 1);
        assert_eq!(half_float(2f32.powi(-25)), 0);
        assert_eq!(half_float(3.0 * 2f32.powi(-25)), 2);
        // 1 + 2^-11 ties to 1, 1 + 3 * 2^-11 to 1 + 2^-9.
        assert_eq!(half_float(1.0 + 2f32.powi(-11)), 0x3c00);
        assert_eq!(half_float(1.0 + 3.0 * 2f32.powi(-11)), 0x3c02);
        for whole in -2048..=2048 {
            let value = f32::from(i16::try_from(whole).unwrap_or(0));
            assert_eq!(from_half_float(half_float(value)), value);
        }
        assert_eq!(
            half_float_pair(vector(-8.0, -4.0)),
            [0x00, 0xc8, 0x00, 0xc4]
        );
    }
}
