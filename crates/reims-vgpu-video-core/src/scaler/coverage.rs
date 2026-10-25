//! What a transform writes of its destination: the bytes core's reference
//! scaler writes for a destination window (`reference::run`), the
//! window's luma and every chroma sample it shares. Outside them the
//! guest's destination is left as it was, by every backend: a backend that
//! writes whole planes copies the rest back, and the conformance suite
//! checks that nothing else changed.

use crate::scaler::format::{Packing, ScalerFormat};

/// A destination window in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// Its left edge.
    pub x: usize,
    /// Its top edge.
    pub y: usize,
    /// Its width.
    pub width: usize,
    /// Its height.
    pub height: usize,
}

/// A run of bytes in one plane: the row, the byte it starts at in the row,
/// and its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// The plane.
    pub plane: usize,
    /// The row in that plane.
    pub row: usize,
    /// The first byte, from the row's start.
    pub start: usize,
    /// The bytes.
    pub length: usize,
}

/// The bytes a transform into `window` writes in a surface of `format`:
/// the window's luma, and every chroma sample it shares, as core's
/// reference writes them (`reference::run`). Outside them the guest's
/// destination is left as it was.
pub fn covered(format: ScalerFormat, window: Window) -> Vec<Run> {
    let Window {
        x,
        y,
        width,
        height,
    } = window;
    let mut runs = Vec::new();
    let s = format.bytes_per_sample();
    match format.packing() {
        Packing::Biplanar420 => {
            for row in y..y + height {
                runs.push(Run {
                    plane: 0,
                    row,
                    start: x * s,
                    length: width * s,
                });
            }
            let (c0, c1) = (x / 2, (x + width).div_ceil(2));
            for row in y / 2..(y + height).div_ceil(2) {
                runs.push(Run {
                    plane: 1,
                    row,
                    start: c0 * 2 * s,
                    length: (c1 - c0) * 2 * s,
                });
            }
        }
        Packing::Packed422(order) => {
            let (c0, c1) = (x / 2, (x + width).div_ceil(2));
            // The bytes of `pair` the window writes, in order: its chroma,
            // and the luma of its pixels inside the window.
            let pair_bytes = |pair: usize| {
                let mut bytes: Vec<usize> = (order.iter().enumerate())
                    .filter(|&(i, _)| {
                        let luma = i == 0 || i == 2;
                        !luma || (x..x + width).contains(&(pair * 2 + i / 2))
                    })
                    .map(|(_, &at)| pair * 4 + at)
                    .collect();
                bytes.sort_unstable();
                bytes
            };
            // Pairs inside the window are written whole, so a row is one
            // run but for a partial pair at either end: a few runs a row,
            // not one a byte.
            for row in y..y + height {
                let first = runs.len();
                let mut add = |start: usize, length: usize| {
                    let in_row = runs.len() > first;
                    match runs.last_mut() {
                        Some(last) if in_row && last.start + last.length == start => {
                            last.length += length;
                        }
                        _ => runs.push(Run {
                            plane: 0,
                            row,
                            start,
                            length,
                        }),
                    }
                };
                if c1 > c0 {
                    pair_bytes(c0).into_iter().for_each(|at| add(at, 1));
                }
                if c1 > c0 + 2 {
                    add((c0 + 1) * 4, (c1 - c0 - 2) * 4);
                }
                if c1 > c0 + 1 {
                    pair_bytes(c1 - 1).into_iter().for_each(|at| add(at, 1));
                }
            }
        }
        Packing::Rgb32(_) => {
            for row in y..y + height {
                runs.push(Run {
                    plane: 0,
                    row,
                    start: x * 4,
                    length: width * 4,
                });
            }
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_covers_its_chroma_and_only_its_luma() {
        let runs = covered(
            ScalerFormat::Yuyv,
            Window {
                x: 1,
                y: 0,
                width: 2,
                height: 1,
            },
        );
        // Pixels 1 and 2: pair 0's Y1 and chroma, pair 1's Y0 and chroma,
        // bytes 1 to 5 and 7.
        let spans: Vec<(usize, usize)> = runs.iter().map(|r| (r.start, r.length)).collect();
        assert_eq!(spans, vec![(1, 5), (7, 1)]);
        let nv12 = covered(
            ScalerFormat::Nv12VideoRange,
            Window {
                x: 3,
                y: 1,
                width: 2,
                height: 2,
            },
        );
        assert_eq!(nv12.len(), 4);
        assert_eq!(
            nv12[2],
            Run {
                plane: 1,
                row: 0,
                start: 2,
                length: 4
            }
        );
    }

    /// The bytes of a 4:2:2 window one at a time, as the reference
    /// writes them.
    fn bytes_422(order: [usize; 4], window: Window) -> Vec<(usize, usize)> {
        let mut bytes = Vec::new();
        let (c0, c1) = (window.x / 2, (window.x + window.width).div_ceil(2));
        for row in window.y..window.y + window.height {
            for pair in c0..c1 {
                for (i, &at) in order.iter().enumerate() {
                    let pixel = pair * 2 + i / 2;
                    let inside = (window.x..window.x + window.width).contains(&pixel);
                    if (i != 0 && i != 2) || inside {
                        bytes.push((row, pair * 4 + at));
                    }
                }
            }
        }
        bytes.sort_unstable();
        bytes
    }

    #[test]
    fn a_422_window_is_a_few_runs_a_row_over_the_same_bytes() {
        for (format, order) in [
            (ScalerFormat::Yuyv, [0, 1, 2, 3]),
            (ScalerFormat::Uyvy, [1, 0, 3, 2]),
        ] {
            for x in 0..5 {
                for width in 0..7 {
                    let window = Window {
                        x,
                        y: 1,
                        width,
                        height: 2,
                    };
                    let mut bytes: Vec<(usize, usize)> = covered(format, window)
                        .iter()
                        .flat_map(|r| (r.start..r.start + r.length).map(move |at| (r.row, at)))
                        .collect();
                    bytes.sort_unstable();
                    assert_eq!(bytes, bytes_422(order, window), "{format:?} {window:?}");
                }
            }
        }
        // A 16384x16384 window: three runs a row at most, not 2^29.
        let whole = Window {
            x: 1,
            y: 0,
            width: 16383,
            height: 16384,
        };
        assert!(covered(ScalerFormat::Yuyv, whole).len() <= 3 * 16384);
    }
}
