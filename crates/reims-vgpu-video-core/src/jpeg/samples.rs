//! Writing a decoded JPEG's samples into the engine's layouts, for the
//! backends: each decoder's planes come in a layout of its own, which a
//! [`SampleView`] per component describes, and [`write_samples`] copies the
//! request's region of them into the destination.
//!
//! It moves samples and does no arithmetic on them: a layout that needs
//! the chroma resampled or the colour converted (a 4:4:4 stream into
//! `420f`, any stream into `BGRA`) is refused as
//! [`JpegError::UnsupportedFormat`], for the backend to do on its hardware
//! or refuse. A backend that has, resampled to the layout's sampling or
//! converted to BGRA, hands its result to [`write_samples`] or
//! [`write_bgra`] as a whole image. Grey goes into every YCbCr layout,
//! with chroma 0x80, as Apple's host writes it.

use super::{JpegDecode, JpegError, JpegOutputFormat, JpegSubsampling};
use crate::surfaces::PlaneDestination;

/// What a grey image's chroma is written as: no colour, at full range.
const NEUTRAL_CHROMA: u8 = 0x80;

/// One component's samples, as a backend holds them: the sample at (x, y)
/// is `bytes[offset + y * pitch + x * step]`.
#[derive(Clone, Copy, Debug)]
pub struct SampleView<'a> {
    /// The memory the samples are in.
    pub bytes: &'a [u8],
    /// Where the first sample is.
    pub offset: usize,
    /// The bytes from one row to the next.
    pub pitch: usize,
    /// The bytes from one sample to the next in a row: 1 for a plane of
    /// its own, 2 for NV12's interleaved chroma, 4 for YUY2's.
    pub step: usize,
}

impl SampleView<'_> {
    /// Where the sample at (x, y) is, unless that is past the address
    /// space.
    fn at(&self, x: usize, y: usize) -> Option<usize> {
        y.checked_mul(self.pitch)?
            .checked_add(x.checked_mul(self.step)?)?
            .checked_add(self.offset)
    }

    /// `count` bytes from the sample at (x, y), where they lie in `bytes`.
    fn bytes_at(&self, x: usize, y: usize, count: usize) -> Option<&[u8]> {
        let start = self.at(x, y)?;
        self.bytes.get(start..start.checked_add(count)?)
    }

    /// `count` samples of row `y` from column `x`, where they are
    /// contiguous.
    fn contiguous(&self, x: usize, y: usize, count: usize) -> Option<&[u8]> {
        if self.step != 1 {
            return None;
        }
        self.bytes_at(x, y, count)
    }

    /// The sample at (x, y).
    fn get(&self, x: usize, y: usize) -> Result<u8, JpegError> {
        self.at(x, y)
            .and_then(|at| self.bytes.get(at))
            .copied()
            .ok_or_else(|| JpegError::Malfunction("a decoded plane ends early".into()))
    }
}

/// A decoded image's samples, at the stream's own sampling.
#[derive(Clone, Copy, Debug)]
pub struct DecodedSamples<'a> {
    /// How the image is sampled.
    pub subsampling: JpegSubsampling,
    /// Y, at the image's size.
    pub luma: SampleView<'a>,
    /// Cb and Cr, at the subsampling's size; none for grey.
    pub chroma: Option<[SampleView<'a>; 2]>,
}

/// Writes `request.region` of `samples` into `planes`, in `request.format`,
/// from their top-left corners. A layout whose chroma is sampled otherwise
/// than the stream's, or that is not YCbCr, is
/// [`JpegError::UnsupportedFormat`], and a scale other than 1
/// [`JpegError::UnsupportedScale`]: each would change the samples.
/// Nothing is written then.
pub fn write_samples(
    samples: &DecodedSamples<'_>,
    request: &JpegDecode<'_>,
    planes: &mut [PlaneDestination<'_>],
) -> Result<(), JpegError> {
    if request.scale != 1 {
        return Err(JpegError::UnsupportedScale(request.scale));
    }
    let not_ycbcr =
        || JpegError::UnsupportedFormat(format!("{:?}, which is not YCbCr", request.format));
    let Some(divisors) = request.format.chroma_divisors() else {
        return Err(not_ycbcr());
    };
    let chroma = match (samples.chroma, samples.subsampling.chroma_divisors()) {
        (None, None) => None,
        (Some(chroma), Some(sampled)) if sampled == divisors => Some(chroma),
        (Some(_), Some(_)) => {
            return Err(JpegError::UnsupportedFormat(format!(
                "{:?} from a {:?} stream, which resamples its chroma",
                request.format, samples.subsampling
            )));
        }
        _ => {
            return Err(JpegError::Malfunction(
                "decoded chroma that is not the stream's".into(),
            ));
        }
    };
    let region = Region::of(request, divisors);
    match request.format {
        JpegOutputFormat::Biplanar420
        | JpegOutputFormat::Biplanar422
        | JpegOutputFormat::Biplanar444 => {
            let [luma_plane, chroma_plane] = planes else {
                return Err(JpegError::Malfunction("not two planes".into()));
            };
            write_luma(&samples.luma, &region, luma_plane)?;
            write_interleaved_chroma(chroma.as_ref(), &region, chroma_plane)
        }
        JpegOutputFormat::Yuyv | JpegOutputFormat::Uyvy => {
            let [plane] = planes else {
                return Err(JpegError::Malfunction("not one plane".into()));
            };
            let order = if request.format == JpegOutputFormat::Yuyv {
                PackedOrder::LumaFirst
            } else {
                PackedOrder::ChromaFirst
            };
            write_packed(&samples.luma, chroma.as_ref(), &region, order, plane)
        }
        JpegOutputFormat::Bgra => Err(not_ycbcr()),
    }
}

/// Writes the top-left `width` by `height` pixels of `pixels`, BGRA four
/// bytes each, into the top-left corner of `planes`, a `BGRA` surface's
/// one plane. As [`write_samples`], it moves bytes: the conversion to RGB,
/// and any scale, are the backend's.
pub fn write_bgra(
    pixels: &SampleView<'_>,
    (width, height): (u32, u32),
    planes: &mut [PlaneDestination<'_>],
) -> Result<(), JpegError> {
    let [plane] = planes else {
        return Err(JpegError::Malfunction("not one plane".into()));
    };
    if pixels.step != 4 {
        return Err(JpegError::Malfunction(
            "BGRA pixels that are not four bytes each".into(),
        ));
    }
    let size = |value: u32| usize::try_from(value).unwrap_or(usize::MAX);
    let (width, height) = (size(width), size(height));
    let row_bytes = width.saturating_mul(4);
    check_plane(plane, height, row_bytes)?;
    if height > 1 && pixels.pitch < row_bytes {
        return Err(JpegError::Malfunction(
            "a converted image's rows overlap".into(),
        ));
    }
    for row in 0..height {
        let from = pixels
            .bytes_at(0, row, row_bytes)
            .ok_or_else(|| JpegError::Malfunction("a converted image ends early".into()))?;
        row_of(plane, row, row_bytes)?.copy_from_slice(from);
    }
    Ok(())
}

/// The region in luma and chroma samples.
struct Region {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    chroma_x: usize,
    chroma_y: usize,
    chroma_width: usize,
    chroma_height: usize,
}

impl Region {
    fn of(request: &JpegDecode<'_>, (across, down): (u32, u32)) -> Self {
        let size = |value: u32| usize::try_from(value).unwrap_or(usize::MAX);
        let region = request.region;
        // The region starts on an MCU, so its chroma starts on a whole
        // chroma sample.
        Self {
            x: size(region.x),
            y: size(region.y),
            width: size(region.width),
            height: size(region.height),
            chroma_x: size(region.x / across),
            chroma_y: size(region.y / down),
            chroma_width: size(region.width.div_ceil(across)),
            chroma_height: size(region.height.div_ceil(down)),
        }
    }
}

/// Checks that `plane` has `rows` rows of `row_bytes`.
fn check_plane(
    plane: &PlaneDestination<'_>,
    rows: usize,
    row_bytes: usize,
) -> Result<(), JpegError> {
    let fits = usize::try_from(plane.rows).is_ok_and(|have| have >= rows)
        && plane.bytes_per_row >= row_bytes
        && (plane.bytes_per_row.checked_mul(rows.saturating_sub(1)))
            .and_then(|before| before.checked_add(row_bytes))
            .is_some_and(|needed| plane.bytes.len() >= needed);
    if fits { Ok(()) } else { Err(too_small()) }
}

/// What a destination plane too small for the output is.
fn too_small() -> JpegError {
    JpegError::Malfunction("a destination plane smaller than the output".into())
}

/// Row `row` of `plane`, `row_bytes` long: within it, once [`check_plane`]
/// has passed it for the row.
fn row_of<'p>(
    plane: &'p mut PlaneDestination<'_>,
    row: usize,
    row_bytes: usize,
) -> Result<&'p mut [u8], JpegError> {
    let start = row.checked_mul(plane.bytes_per_row).ok_or_else(too_small)?;
    let end = start.checked_add(row_bytes).ok_or_else(too_small)?;
    plane.bytes.get_mut(start..end).ok_or_else(too_small)
}

fn write_luma(
    luma: &SampleView<'_>,
    region: &Region,
    plane: &mut PlaneDestination<'_>,
) -> Result<(), JpegError> {
    check_plane(plane, region.height, region.width)?;
    for row in 0..region.height {
        let to = row_of(plane, row, region.width)?;
        if let Some(from) = luma.contiguous(region.x, region.y + row, region.width) {
            to.copy_from_slice(from);
            continue;
        }
        for (column, sample) in to.iter_mut().enumerate() {
            *sample = luma.get(region.x + column, region.y + row)?;
        }
    }
    Ok(())
}

/// Cb and Cr in pairs, or 0x80 for grey.
fn write_interleaved_chroma(
    chroma: Option<&[SampleView<'_>; 2]>,
    region: &Region,
    plane: &mut PlaneDestination<'_>,
) -> Result<(), JpegError> {
    let row_bytes = region.chroma_width.saturating_mul(2);
    check_plane(plane, region.chroma_height, row_bytes)?;
    for row in 0..region.chroma_height {
        let to = row_of(plane, row, row_bytes)?;
        let Some([cb, cr]) = chroma else {
            to.fill(NEUTRAL_CHROMA);
            continue;
        };
        let y = region.chroma_y + row;
        // NV12's chroma is already in pairs.
        let paired = cb.step == 2 && cr.step == 2 && cr.pitch == cb.pitch;
        if paired
            && cb.offset.checked_add(1) == Some(cr.offset)
            && let Some(from) = cb.bytes_at(region.chroma_x, y, row_bytes)
        {
            to.copy_from_slice(from);
            continue;
        }
        for (pair, samples) in to.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let x = region.chroma_x + pair;
            *samples = [cb.get(x, y)?, cr.get(x, y)?];
        }
    }
    Ok(())
}

/// Which comes first in a packed 4:2:2 pair.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PackedOrder {
    /// `yuvs`: Y0 Cb Y1 Cr.
    LumaFirst,
    /// `2vuy`: Cb Y0 Cr Y1.
    ChromaFirst,
}

/// Pairs of pixels in four bytes each. An odd width's last pair repeats
/// its one luma sample.
fn write_packed(
    luma: &SampleView<'_>,
    chroma: Option<&[SampleView<'_>; 2]>,
    region: &Region,
    order: PackedOrder,
    plane: &mut PlaneDestination<'_>,
) -> Result<(), JpegError> {
    let pairs = region.width.div_ceil(2);
    let row_bytes = pairs.saturating_mul(4);
    check_plane(plane, region.height, row_bytes)?;
    let last = region.width.saturating_sub(1);
    for row in 0..region.height {
        let y = region.y + row;
        let to = row_of(plane, row, row_bytes)?;
        for (pair, pixels) in to.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let first = luma.get(region.x + 2 * pair, y)?;
            let second = luma.get(region.x + (2 * pair + 1).min(last), y)?;
            let (cb, cr) = match chroma {
                Some([cb, cr]) => (
                    cb.get(region.chroma_x + pair, region.chroma_y + row)?,
                    cr.get(region.chroma_x + pair, region.chroma_y + row)?,
                ),
                None => (NEUTRAL_CHROMA, NEUTRAL_CHROMA),
            };
            *pixels = match order {
                PackedOrder::LumaFirst => [first, cb, second, cr],
                PackedOrder::ChromaFirst => [cb, first, cr, second],
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
