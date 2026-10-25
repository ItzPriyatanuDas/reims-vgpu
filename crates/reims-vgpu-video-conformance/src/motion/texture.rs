//! Frames with known motion, written from scratch: a texture defined at
//! every real point, so that a frame of it moved by any vector, whole or
//! fractional, can be drawn, and a motion estimator's answer checked
//! against the vector.
//!
//! The texture is value noise of our own: a hash of each lattice point
//! gives it a level, and the levels are interpolated bilinearly between
//! them. Two octaves, a coarse one for the shape a block search locks onto
//! and a fine one for detail, keep every 4x4 block distinct.

/// The coarse lattice's spacing, in pixels.
const COARSE_CELL: f64 = 8.0;
/// The fine lattice's spacing, in pixels.
const FINE_CELL: f64 = 3.0;

/// A level from 0 to 1 for the lattice point (`x`, `y`) of one octave.
fn lattice(x: i64, y: i64, octave: u64) -> f64 {
    // SplitMix64's finalizer over the point and the octave.
    let mut h = (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (y as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ octave.wrapping_mul(0x1656_67b1_9e37_79f9);
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// One octave at (`x`, `y`), with lattice points `cell` pixels apart.
fn octave(x: f64, y: f64, cell: f64, octave: u64) -> f64 {
    let (u, v) = (x / cell, y / cell);
    let (x0, y0) = (u.floor(), v.floor());
    let (fx, fy) = (u - x0, v - y0);
    let (x0, y0) = (x0 as i64, y0 as i64);
    let top = lattice(x0, y0, octave) * (1.0 - fx) + lattice(x0 + 1, y0, octave) * fx;
    let bottom = lattice(x0, y0 + 1, octave) * (1.0 - fx) + lattice(x0 + 1, y0 + 1, octave) * fx;
    top * (1.0 - fy) + bottom * fy
}

/// The texture's luma at (`x`, `y`), within video range, 16 to 235.
pub fn luma(x: f64, y: f64) -> u8 {
    let level = 0.7 * octave(x, y, COARSE_CELL, 1) + 0.3 * octave(x, y, FINE_CELL, 2);
    (16.0 + 219.0 * level).round().clamp(16.0, 235.0) as u8
}

/// A `width` by `height` luma plane, `bytes_per_row` apart, whose content
/// is the texture moved by (`dx`, `dy`) pixels: its pixel (x, y) is the
/// texture at (x - dx, y - dy), so that a motion estimator comparing it
/// with the unmoved plane finds (-dx, -dy), the way back to the
/// reference.
pub fn moved_luma(width: usize, height: usize, bytes_per_row: usize, dx: f64, dy: f64) -> Vec<u8> {
    let mut plane = vec![0u8; bytes_per_row * height];
    for (y, row) in plane.chunks_exact_mut(bytes_per_row.max(1)).enumerate() {
        for (x, sample) in row.iter_mut().take(width).enumerate() {
            *sample = luma(x as f64 - dx, y as f64 - dy);
        }
    }
    plane
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moved_plane_is_the_plane_shifted() {
        let still = moved_luma(32, 16, 32, 0.0, 0.0);
        let moved = moved_luma(32, 16, 32, 3.0, -2.0);
        for y in 2..14 {
            for x in 3..32 {
                assert_eq!(moved[y * 32 + x], still[(y + 2) * 32 + x - 3]);
            }
        }
    }

    #[test]
    fn the_texture_stays_in_video_range_and_varies() {
        let plane = moved_luma(64, 64, 64, 0.0, 0.0);
        assert!(plane.iter().all(|&v| (16..=235).contains(&v)));
        let (min, max) = (plane.iter().min(), plane.iter().max());
        assert!(max.zip(min).is_some_and(|(max, min)| max - min > 100));
        // No two 4x4 blocks of the plane are alike.
        let block = |bx: usize, by: usize| -> Vec<u8> {
            (0..4)
                .flat_map(|y| (0..4).map(move |x| (x, y)))
                .map(|(x, y)| plane[(by * 4 + y) * 64 + bx * 4 + x])
                .collect()
        };
        let blocks: Vec<_> = (0..16)
            .flat_map(|by| (0..16).map(move |bx| (bx, by)))
            .map(|(bx, by)| block(bx, by))
            .collect();
        for (i, a) in blocks.iter().enumerate() {
            assert!(blocks[i + 1..].iter().all(|b| b != a));
        }
    }
}
