//! What the decoder makes on the CPU of nvJPEG's planes, where they are not
//! the output as core's reference has it: planes resampled, and BGRA made
//! of YCbCr or of nvJPEG's BGR.
//!
//! nvJPEG scales each 8x8 block alone, so at 1/2, 1/4 or 1/8 each sample it
//! writes covers that many of the image's, counted from the image's
//! top-left corner, and it writes each component at its own sampling: NV12
//! only from 4:2:0 and YUY2 only from 4:2:2 (`NVJPEG_STATUS_INVALID_PARAMETER`
//! from the others; RTX 5080 Laptop GPU, libnvjpeg 13.2.3, 2026-10-25).
//! Core's reference (`reims_vgpu_video_core::scaler::reference`, which the
//! conformance compares with) spreads the output's samples over the region
//! instead: `ceil(width / scale)` of them over `width` of the image's, and
//! the chroma's over the region divided by the stream's chroma divisors, at
//! the layout's sampling. The two agree where each side of the window is
//! the scale times the output's ([`Axis::is_copy`]), and the decoder then
//! copies nvJPEG's samples. It copies them too at scale 1 into a layout of
//! the stream's sampling, whose output is the image's own samples, as the
//! conformance compares it sample for sample, though the reference would
//! stretch an odd size's chroma window (6.5 samples into 7). Elsewhere (an
//! odd size at a scale, a layout of another sampling, at any scale) each
//! output sample here is the reference's filter run
//! on nvJPEG's samples, each taken as the mean of the image's samples it
//! covers, which nvJPEG's scaled decodes were measured within 0.15 of on
//! average: coverage where an output sample covers more than one of the
//! image's samples, linear interpolation between centres where it covers
//! less, the window's edge samples repeated beyond it.
//!
//! nvJPEG's own BGR at a scale scored 21.1 to 28.6 dB per channel against
//! the reference with chroma interpolated from its centres, and 22.8 to
//! 32.5 dB with the nearest chroma (4:2:0 at 1/2, 4:2:2 at 1/4, a crop of
//! 4:2:0 at 1/4; same GPU and library), where its planes at the same scales
//! scored 57 to 67 dB. So BGRA at a scale is made here of the planes:
//! BT.601 at full range, as JFIF's samples are, each pixel taking the
//! chroma sample it lies in, the nearest, one of the two the conformance
//! takes as Apple's. At scale 1 nvJPEG's BGR, its chroma interpolated,
//! scored 59 to 63 dB, and is made BGRA as it is.

use std::ops::Range;

use reims_vgpu_video_core::jpeg::JpegError;
use reims_vgpu_video_core::surfaces::PlaneDestination;

/// One plane's samples, `width` a row with no padding.
#[derive(Clone, Copy, Debug)]
pub(super) struct Plane<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) width: usize,
    pub(super) height: usize,
}

/// One axis of an output plane made of nvJPEG's samples: the window of the
/// image's plane it covers, in that plane's samples, and which samples of
/// nvJPEG's it is made of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Axis {
    /// Where the window starts.
    pub(super) start: f64,
    /// The window's length.
    pub(super) length: f64,
    /// Where it ends, within the plane.
    pub(super) end: f64,
    /// Where the first of nvJPEG's samples here starts.
    pub(super) base: f64,
    /// How many of the image's samples each of nvJPEG's covers: the scale.
    pub(super) unit: f64,
    /// nvJPEG's samples here.
    pub(super) source: usize,
    /// The output's samples.
    pub(super) output: usize,
}

impl Axis {
    /// Whether output sample `i` is nvJPEG's sample `i`: the window starts
    /// where one does, and each output sample covers one of nvJPEG's.
    pub(super) fn is_copy(&self) -> bool {
        self.start == self.base
            && self.length == self.unit * self.output as f64
            && self.source == self.output
    }
}

/// An output of `output` samples over `(start, length)` of a plane `plane`
/// samples long, made of nvJPEG's `decoded` samples, which start at
/// `origin` of the plane and each cover `unit`: which of them to copy back,
/// and the axis made of those. With `samples`, the output is nvJPEG's
/// samples from the window's start, whatever its length. None where the
/// window is not in them.
pub(super) fn axis(
    (start, length): (f64, f64),
    plane: f64,
    (origin, unit): (usize, f64),
    (decoded, output): (usize, usize),
    samples: bool,
) -> Option<(Range<usize>, Axis)> {
    let origin = origin as f64;
    let first = (start - origin) / unit;
    if output == 0 || first < 0.0 || first.is_nan() || length <= 0.0 {
        return None;
    }
    let end = (start + length).min(plane);
    let exact = first.fract() == 0.0 && (samples || length == unit * output as f64);
    let copied = if exact {
        let first = first as usize;
        first..first.checked_add(output)?
    } else {
        let last = ((end - origin) / unit).ceil();
        (first.floor() as usize)..(last as usize).min(decoded)
    };
    if copied.is_empty() || copied.end > decoded {
        return None;
    }
    let axis = Axis {
        start,
        length,
        end,
        base: origin + copied.start as f64 * unit,
        unit,
        source: copied.len(),
        output,
    };
    Some((copied, axis))
}

/// Which source samples each output sample along `axis` is made of, with
/// weights summing to 1: the reference's filter (module docs).
fn taps(axis: &Axis) -> Vec<Vec<(usize, f32)>> {
    let Axis {
        start,
        length,
        end,
        base,
        unit,
        source,
        output,
    } = *axis;
    let ratio = length / output as f64;
    let at = |position: f64| (position - base) / unit;
    let first = at(start).floor().max(0.0);
    let last = (at(end).ceil() - 1.0)
        .min(source.saturating_sub(1) as f64)
        .max(first);
    let index = |i: f64| i.clamp(first, last) as usize;
    (0..output)
        .map(|d| {
            let centre = start + (d as f64 + 0.5) * ratio;
            let mut taps: Vec<(usize, f64)> = Vec::with_capacity(3);
            if ratio <= 1.0 {
                let s = at(centre) - 0.5;
                let s0 = s.floor();
                let a = s - s0;
                taps.extend([(index(s0), 1.0 - a), (index(s0 + 1.0), a)]);
            } else {
                let from = (centre - ratio / 2.0).max(start);
                let to = (centre + ratio / 2.0).min(end);
                let mut i = at(from).floor();
                while base + i * unit < to {
                    let low = base + i * unit;
                    let covered = (low + unit).min(to) - low.max(from);
                    if covered > 0.0 {
                        taps.push((index(i), covered));
                    }
                    i += 1.0;
                }
                if taps.is_empty() {
                    taps.push((index(at(centre).floor()), 1.0));
                }
            }
            let total: f64 = taps.iter().map(|&(_, weight)| weight).sum();
            taps.into_iter()
                .filter(|&(_, weight)| weight > 0.0)
                .map(|(i, weight)| (i, (weight / total) as f32))
                .collect()
        })
        .collect()
}

/// `plane`, whose size is `across.source` by `down.source`, made
/// `across.output` by `down.output` samples as the axes say: across each
/// row first, then down.
pub(super) fn resample(plane: Plane<'_>, across: &Axis, down: &Axis) -> Result<Vec<u8>, JpegError> {
    let (width, height) = (across.output, down.output);
    let fits = plane.width == across.source
        && plane.height == down.source
        && plane.width > 0
        && width > 0
        && plane
            .width
            .checked_mul(plane.height)
            .is_some_and(|len| plane.bytes.len() >= len);
    if !fits {
        return Err(malfunction());
    }
    let (columns, rows) = (taps(across), taps(down));
    // Each tap's index is below its axis's source (`taps`), so the indexing
    // below stays in the plane and in `widened`.
    let mut widened = vec![0f32; width.saturating_mul(plane.height)];
    for (row, out) in plane
        .bytes
        .chunks_exact(plane.width)
        .zip(widened.chunks_exact_mut(width))
    {
        for (sample, taps) in out.iter_mut().zip(&columns) {
            *sample = taps.iter().map(|&(i, w)| f32::from(row[i]) * w).sum();
        }
    }
    let mut out = vec![0u8; width.saturating_mul(height)];
    let mut sums = vec![0f32; width];
    for (out_row, taps) in out.chunks_exact_mut(width).zip(&rows) {
        sums.fill(0.0);
        for &(i, w) in taps {
            let from = &widened[i * width..(i + 1) * width];
            for (sum, sample) in sums.iter_mut().zip(from) {
                *sum += sample * w;
            }
        }
        for (sample, sum) in out_row.iter_mut().zip(&sums) {
            *sample = (sum + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    Ok(out)
}

/// The chroma planes of an image made BGRA, Cb then Cr, `divisors` coarser
/// than its luma, rows packed.
#[derive(Clone, Copy, Debug)]
pub(super) struct Chroma<'a> {
    pub(super) cb: &'a [u8],
    pub(super) cr: &'a [u8],
    pub(super) divisors: (usize, usize),
}

/// Writes `luma`, `width` by `height` samples, and `chroma` (none for grey)
/// into the top-left corner of `plane` as BGRA: BT.601 at full range, each
/// pixel taking the chroma sample it lies in, each alpha 0xff. libjpeg's
/// arithmetic, its constants in 16 fractional bits.
pub(super) fn write_ycbcr_bgra(
    luma: &[u8],
    chroma: Option<Chroma<'_>>,
    (width, height): (usize, usize),
    plane: &mut PlaneDestination<'_>,
) -> Result<(), JpegError> {
    const HALF: i32 = 1 << 15;
    let (across, down) = chroma.map_or((1, 1), |c| (c.divisors.0.max(1), c.divisors.1.max(1)));
    let chroma_width = width.div_ceil(across);
    let chroma_len = chroma_width.saturating_mul(height.div_ceil(down));
    let short = luma.len() < width.saturating_mul(height)
        || chroma.is_some_and(|c| c.cb.len() < chroma_len || c.cr.len() < chroma_len);
    if short || width == 0 {
        return Err(malfunction());
    }
    for (y, (to, from)) in rows(plane, width, height)?
        .zip(luma.chunks_exact(width))
        .enumerate()
    {
        for (x, (pixel, &luma)) in to.as_chunks_mut::<4>().0.iter_mut().zip(from).enumerate() {
            let luma = i32::from(luma);
            let (b, g, r) = match chroma {
                None => (luma, luma, luma),
                Some(c) => {
                    // Below `chroma_len`, as `y` is below `height` and `x`
                    // below `width`.
                    let at = (y / down) * chroma_width + x / across;
                    let cb = i32::from(c.cb[at]) - 128;
                    let cr = i32::from(c.cr[at]) - 128;
                    (
                        luma + ((116_130 * cb + HALF) >> 16),
                        luma + ((-22_554 * cb - 46_802 * cr + HALF) >> 16),
                        luma + ((91_881 * cr + HALF) >> 16),
                    )
                }
            };
            let clamp = |v: i32| v.clamp(0, 255) as u8;
            *pixel = [clamp(b), clamp(g), clamp(r), 0xff];
        }
    }
    Ok(())
}

/// Writes `bgr`, `width` by `height` pixels of B, G and R with no padding,
/// into the top-left corner of `plane` as BGRA, each alpha 0xff.
pub(super) fn write_bgra(
    bgr: &[u8],
    (width, height): (usize, usize),
    plane: &mut PlaneDestination<'_>,
) -> Result<(), JpegError> {
    let source_row = width.checked_mul(3).ok_or_else(too_small)?;
    if width == 0 || bgr.len() < source_row.saturating_mul(height) {
        return Err(malfunction());
    }
    for (to, from) in rows(plane, width, height)?.zip(bgr.chunks_exact(source_row)) {
        for (pixel, &[b, g, r]) in to
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(from.as_chunks::<3>().0)
        {
            *pixel = [b, g, r, 0xff];
        }
    }
    Ok(())
}

/// The first `height` rows of `plane`, each `width` BGRA pixels, once it
/// is checked to hold them.
fn rows<'p>(
    plane: &'p mut PlaneDestination<'_>,
    width: usize,
    height: usize,
) -> Result<impl Iterator<Item = &'p mut [u8]>, JpegError> {
    let row_bytes = width.checked_mul(4).ok_or_else(too_small)?;
    let fits = usize::try_from(plane.rows).is_ok_and(|rows| rows >= height)
        && plane.bytes_per_row >= row_bytes
        && plane
            .bytes_per_row
            .checked_mul(height.saturating_sub(1))
            .and_then(|before| before.checked_add(row_bytes))
            .is_some_and(|needed| plane.bytes.len() >= needed);
    if !fits || row_bytes == 0 {
        return Err(too_small());
    }
    let pitch = plane.bytes_per_row;
    Ok(plane
        .bytes
        .chunks_mut(pitch)
        .take(height)
        .map(move |row| &mut row[..row_bytes]))
}

fn malfunction() -> JpegError {
    JpegError::Malfunction("a decoded plane ends early".into())
}

fn too_small() -> JpegError {
    JpegError::Malfunction("a destination plane smaller than the output".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The axis of `output` samples over the whole of a plane `length`
    /// long, made of samples each covering `unit`.
    fn whole(length: usize, unit: usize, output: usize) -> Axis {
        let decoded = length.div_ceil(unit);
        let whole = (0.0, length as f64);
        let Some((copied, axis)) = axis(
            whole,
            length as f64,
            (0, unit as f64),
            (decoded, output),
            false,
        ) else {
            unreachable!("an axis");
        };
        assert_eq!(copied, 0..decoded);
        axis
    }

    /// One row of `samples` along `axis`.
    fn across(samples: &[u8], axis: &Axis) -> Result<Vec<u8>, JpegError> {
        let one = whole(1, 1, 1);
        let plane = Plane {
            bytes: samples,
            width: samples.len(),
            height: 1,
        };
        resample(plane, axis, &one)
    }

    #[test]
    fn a_window_of_whole_samples_is_copied() {
        // 640 at 1/2: nvJPEG's 320 samples are the output's.
        assert!(whole(640, 2, 320).is_copy());
        assert!(whole(5, 1, 5).is_copy());
        // 333 at 1/2: 167 samples over 333, not 334.
        assert!(!whole(333, 2, 167).is_copy());
        // A crop at 1/4 from (240, 160): from nvJPEG's sample 60, 40 of them.
        let cropped = axis((240.0, 160.0), 640.0, (0, 4.0), (160, 40), false);
        let Some((copied, axis)) = cropped else {
            unreachable!("an axis");
        };
        assert_eq!(copied, 60..100);
        assert!(axis.is_copy());
        // A window past nvJPEG's samples is none.
        assert!(super::axis((240.0, 160.0), 640.0, (0, 4.0), (80, 40), false).is_none());
        // The chroma of a 13-pixel crop of 4:2:0 at scale 1, from 320:
        // nvJPEG's 7 samples, which the reference would stretch, and which
        // are copied as the stream's own samples.
        let edge = |samples| super::axis((160.0, 6.5), 167.0, (160, 1.0), (7, 7), samples);
        assert!(edge(false).is_some_and(|(copied, axis)| copied == (0..7) && !axis.is_copy()));
        assert!(edge(true).is_some_and(|(copied, _)| copied == (0..7)));
    }

    #[test]
    fn a_copy_resamples_to_itself() {
        assert_eq!(across(&[10, 20, 30], &whole(6, 2, 3)), Ok(vec![10, 20, 30]));
    }

    #[test]
    fn a_shrink_covers_the_window() {
        // A halving averages pairs.
        assert_eq!(across(&[10, 20, 30, 40], &whole(4, 1, 2)), Ok(vec![15, 35]));
        // 5 into 3, as 4:4:4's chroma into 4:2:0's at an odd width: each
        // output sample covers 5/3 of the source's.
        assert_eq!(
            across(&[10, 20, 30, 40, 50], &whole(5, 1, 3)),
            Ok(vec![14, 30, 46])
        );
    }

    #[test]
    fn an_odd_size_at_a_scale_covers_the_scaled_samples() {
        // 5 samples at 1/2: nvJPEG's three cover [0, 2), [2, 4) and [4, 6);
        // the output's three cover 5/3 each.
        assert_eq!(across(&[10, 30, 50], &whole(5, 2, 3)), Ok(vec![10, 26, 42]));
    }

    #[test]
    fn a_doubling_interpolates_between_the_centres() {
        // 4:2:0's chroma to 4:2:2's: rows at 3/4 and 1/4, held at the edges.
        assert_eq!(
            across(&[0, 100, 200], &whole(3, 1, 6)),
            Ok(vec![0, 25, 75, 125, 175, 200])
        );
    }

    #[test]
    fn a_plane_of_another_size_than_its_axes_is_refused() {
        let plane = Plane {
            bytes: &[0; 4],
            width: 4,
            height: 1,
        };
        assert!(resample(plane, &whole(6, 2, 3), &whole(1, 1, 1)).is_err());
    }

    fn destination(bytes: &mut [u8], bytes_per_row: usize, rows: u32) -> PlaneDestination<'_> {
        PlaneDestination {
            bytes,
            bytes_per_row,
            rows,
        }
    }

    #[test]
    fn ycbcr_becomes_bgra_with_the_nearest_chroma() {
        // A 3x2 image of 4:2:0: two chroma samples across, one down.
        let luma = [100, 100, 100, 128, 128, 128];
        let (cb, cr) = ([128, 128], [128, 200]);
        let chroma = Chroma {
            cb: &cb,
            cr: &cr,
            divisors: (2, 2),
        };
        let mut bytes = [9u8; 2 * 16];
        let written = write_ycbcr_bgra(
            &luma,
            Some(chroma),
            (3, 2),
            &mut destination(&mut bytes, 16, 2),
        );
        assert_eq!(written, Ok(()));
        // Cr 200 is R + 101 and G - 51 (BT.601: 1.402 and 0.714 of +72).
        let grey = |y| [y, y, y, 255];
        let red = |y: u8| [y, y - 51, y + 101, 255];
        let row = |pixels: [[u8; 4]; 3]| pixels.concat();
        assert_eq!(&bytes[..12], row([grey(100), grey(100), red(100)]));
        assert_eq!(&bytes[12..16], [9; 4]);
        assert_eq!(&bytes[16..28], row([grey(128), grey(128), red(128)]));
        // Grey: no chroma.
        let written = write_ycbcr_bgra(&luma, None, (3, 2), &mut destination(&mut bytes, 12, 2));
        assert_eq!(written, Ok(()));
        assert_eq!(&bytes[12..24], row([grey(128); 3]));
        // Chroma short of the image's.
        let short = Chroma {
            cb: &cb[..1],
            cr: &cr,
            divisors: (2, 2),
        };
        let refused = write_ycbcr_bgra(
            &luma,
            Some(short),
            (3, 2),
            &mut destination(&mut bytes, 12, 2),
        );
        assert!(refused.is_err());
    }

    #[test]
    fn bgr_becomes_opaque_bgra_in_the_corner() {
        let bgr = [1, 2, 3, 4, 5, 6];
        let mut bytes = [9u8; 24];
        assert_eq!(
            write_bgra(&bgr, (1, 2), &mut destination(&mut bytes, 12, 2)),
            Ok(())
        );
        assert_eq!(
            bytes,
            [
                1, 2, 3, 255, 9, 9, 9, 9, 9, 9, 9, 9, 4, 5, 6, 255, 9, 9, 9, 9, 9, 9, 9, 9
            ]
        );
        let mut small = [0u8; 4];
        assert!(write_bgra(&bgr, (1, 2), &mut destination(&mut small, 4, 1)).is_err());
    }
}
