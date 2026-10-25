//! The writer against small images whose every sample is its own value,
//! held as NV12, YUY2, three planes and one, as VA-API's surfaces hold
//! them.

use super::*;
use crate::jpeg::PixelRect;

/// A request for `region` of a `width` x `height` image.
fn request(
    width: u32,
    height: u32,
    subsampling: JpegSubsampling,
    region: PixelRect,
    format: JpegOutputFormat,
) -> JpegDecode<'static> {
    JpegDecode {
        data: &[],
        width,
        height,
        subsampling,
        region,
        scale: 1,
        format,
    }
}

fn whole(width: u32, height: u32) -> PixelRect {
    PixelRect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// A plane of `rows` rows, `bytes_per_row` apart, filled with 0xEE.
struct Plane {
    bytes: Vec<u8>,
    bytes_per_row: usize,
    rows: u32,
}

impl Plane {
    fn new(bytes_per_row: usize, rows: u32) -> Self {
        Self {
            bytes: vec![0xee; bytes_per_row * usize::try_from(rows).unwrap_or(0)],
            bytes_per_row,
            rows,
        }
    }

    fn destination(&mut self) -> PlaneDestination<'_> {
        PlaneDestination {
            bytes: &mut self.bytes,
            bytes_per_row: self.bytes_per_row,
            rows: self.rows,
        }
    }

    /// Row `row`'s first `bytes`.
    fn row(&self, row: usize, bytes: usize) -> &[u8] {
        &self.bytes[row * self.bytes_per_row..row * self.bytes_per_row + bytes]
    }
}

fn write(
    samples: &DecodedSamples<'_>,
    request: &JpegDecode<'_>,
    planes: &mut [Plane],
) -> Result<(), JpegError> {
    let mut destinations: Vec<PlaneDestination<'_>> =
        planes.iter_mut().map(Plane::destination).collect();
    write_samples(samples, request, &mut destinations)
}

/// The sample at (x, y) of a plane from `base`: `base + 16y + x`, modulo
/// 256.
fn sample(base: u8, x: usize, y: usize) -> u8 {
    base.wrapping_add(u8::try_from((16 * y + x) % 256).unwrap_or(0))
}

/// A plane of its own, of [`sample`]s, rows `pitch` apart.
fn plane(base: u8, width: usize, height: usize, pitch: usize) -> Vec<u8> {
    let mut bytes = vec![0; pitch * height];
    for y in 0..height {
        for x in 0..width {
            bytes[y * pitch + x] = sample(base, x, y);
        }
    }
    bytes
}

fn view(bytes: &[u8], offset: usize, pitch: usize, step: usize) -> SampleView<'_> {
    SampleView {
        bytes,
        offset,
        pitch,
        step,
    }
}

/// NV12 of a `width` x `height` 4:2:0 image: Y from 0, Cb from 0x80 and
/// Cr from 0xC0, at `pitch`.
fn nv12(width: usize, height: usize, pitch: usize) -> Vec<u8> {
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let mut bytes = plane(0, width, height, pitch);
    let (cb, cr) = (plane(0x80, cw, ch, cw), plane(0xc0, cw, ch, cw));
    let mut chroma = vec![0; pitch * ch];
    for y in 0..ch {
        for x in 0..cw {
            chroma[y * pitch + 2 * x] = cb[y * cw + x];
            chroma[y * pitch + 2 * x + 1] = cr[y * cw + x];
        }
    }
    bytes.extend(chroma);
    bytes
}

fn nv12_samples(bytes: &[u8], height: usize, pitch: usize) -> DecodedSamples<'_> {
    let chroma = pitch * height;
    DecodedSamples {
        subsampling: JpegSubsampling::Yuv420,
        luma: view(bytes, 0, pitch, 1),
        chroma: Some([
            view(bytes, chroma, pitch, 2),
            view(bytes, chroma + 1, pitch, 2),
        ]),
    }
}

#[test]
fn nv12_goes_into_420f_row_for_row() -> Result<(), JpegError> {
    let bytes = nv12(6, 4, 8);
    let samples = nv12_samples(&bytes, 4, 8);
    let mut planes = [Plane::new(10, 4), Plane::new(10, 2)];
    let request = request(
        6,
        4,
        JpegSubsampling::Yuv420,
        whole(6, 4),
        JpegOutputFormat::Biplanar420,
    );
    write(&samples, &request, &mut planes)?;
    assert_eq!(planes[0].row(0, 7), [0, 1, 2, 3, 4, 5, 0xee]);
    assert_eq!(planes[0].row(3, 6), [48, 49, 50, 51, 52, 53]);
    assert_eq!(
        planes[1].row(1, 7),
        [0x90, 0xd0, 0x91, 0xd1, 0x92, 0xd2, 0xee]
    );
    Ok(())
}

#[test]
fn a_region_takes_its_own_samples_and_an_odd_edge_its_last_chroma() -> Result<(), JpegError> {
    // A 21x19 image, its MCU at (16, 16): 5x3 of it, and 3x2 of chroma.
    let bytes = nv12(21, 19, 24);
    let samples = nv12_samples(&bytes, 19, 24);
    let mut planes = [Plane::new(5, 3), Plane::new(6, 2)];
    let region = PixelRect {
        x: 16,
        y: 16,
        width: 5,
        height: 3,
    };
    let request = request(
        21,
        19,
        JpegSubsampling::Yuv420,
        region,
        JpegOutputFormat::Biplanar420,
    );
    write(&samples, &request, &mut planes)?;
    assert_eq!(
        planes[0].row(0, 5),
        (16..21).map(|x| sample(0, x, 16)).collect::<Vec<_>>()
    );
    assert_eq!(
        planes[0].row(2, 5),
        (16..21).map(|x| sample(0, x, 18)).collect::<Vec<_>>()
    );
    // Chroma (8, 8) to (10, 9).
    let pairs: Vec<u8> = (8..11)
        .flat_map(|x| [sample(0x80, x, 9), sample(0xc0, x, 9)])
        .collect();
    assert_eq!(planes[1].row(1, 6), pairs);
    Ok(())
}

#[test]
fn three_planes_go_into_444f_in_pairs() -> Result<(), JpegError> {
    let (y, cb, cr) = (
        plane(0, 3, 2, 4),
        plane(0x40, 3, 2, 4),
        plane(0x80, 3, 2, 4),
    );
    let samples = DecodedSamples {
        subsampling: JpegSubsampling::Yuv444,
        luma: view(&y, 0, 4, 1),
        chroma: Some([view(&cb, 0, 4, 1), view(&cr, 0, 4, 1)]),
    };
    let mut planes = [Plane::new(3, 2), Plane::new(6, 2)];
    let request = request(
        3,
        2,
        JpegSubsampling::Yuv444,
        whole(3, 2),
        JpegOutputFormat::Biplanar444,
    );
    write(&samples, &request, &mut planes)?;
    assert_eq!(planes[0].row(1, 3), [16, 17, 18]);
    assert_eq!(planes[1].row(1, 6), [0x50, 0x90, 0x51, 0x91, 0x52, 0x92]);
    Ok(())
}

/// YUY2 of a 3x2 4:2:2 image: Y 0.., Cb 0x80.., Cr 0xC0.., two pairs a
/// row, the last pair's second luma sample padding (0xFF).
fn yuy2() -> Vec<u8> {
    let mut bytes = Vec::new();
    for y in 0u8..2 {
        for pair in 0u8..2 {
            let second = if pair == 1 { 0xff } else { 16 * y + 1 };
            bytes.extend([
                16 * y + 2 * pair,
                0x80 + 16 * y + pair,
                second,
                0xc0 + 16 * y + pair,
            ]);
        }
    }
    bytes
}

fn yuy2_samples(bytes: &[u8]) -> DecodedSamples<'_> {
    DecodedSamples {
        subsampling: JpegSubsampling::Yuv422,
        luma: view(bytes, 0, 8, 2),
        chroma: Some([view(bytes, 1, 8, 4), view(bytes, 3, 8, 4)]),
    }
}

#[test]
fn yuy2_goes_into_yuvs_and_2vuy_with_an_odd_edge_repeated() -> Result<(), JpegError> {
    let bytes = yuy2();
    let samples = yuy2_samples(&bytes);
    let mut yuvs = [Plane::new(8, 2)];
    let format = |format| request(3, 2, JpegSubsampling::Yuv422, whole(3, 2), format);
    write(&samples, &format(JpegOutputFormat::Yuyv), &mut yuvs)?;
    assert_eq!(yuvs[0].row(1, 8), [16, 0x90, 17, 0xd0, 18, 0x91, 18, 0xd1]);
    let mut uyvy = [Plane::new(8, 2)];
    write(&samples, &format(JpegOutputFormat::Uyvy), &mut uyvy)?;
    assert_eq!(uyvy[0].row(0, 8), [0x80, 0, 0xc0, 1, 0x81, 2, 0xc1, 2]);
    let mut biplanar = [Plane::new(3, 2), Plane::new(4, 2)];
    write(
        &samples,
        &format(JpegOutputFormat::Biplanar422),
        &mut biplanar,
    )?;
    assert_eq!(biplanar[0].row(1, 3), [16, 17, 18]);
    assert_eq!(biplanar[1].row(1, 4), [0x90, 0xd0, 0x91, 0xd1]);
    Ok(())
}

#[test]
fn grey_goes_into_every_ycbcr_layout_with_neutral_chroma() -> Result<(), JpegError> {
    let y = plane(0, 3, 3, 3);
    let samples = DecodedSamples {
        subsampling: JpegSubsampling::Grey,
        luma: view(&y, 0, 3, 1),
        chroma: None,
    };
    let format = |format| request(3, 3, JpegSubsampling::Grey, whole(3, 3), format);
    let mut nv12 = [Plane::new(3, 3), Plane::new(4, 2)];
    write(&samples, &format(JpegOutputFormat::Biplanar420), &mut nv12)?;
    assert_eq!(nv12[0].row(2, 3), [32, 33, 34]);
    assert_eq!(nv12[1].row(1, 4), [0x80; 4]);
    let mut biplanar444 = [Plane::new(3, 3), Plane::new(6, 3)];
    write(
        &samples,
        &format(JpegOutputFormat::Biplanar444),
        &mut biplanar444,
    )?;
    assert_eq!(biplanar444[1].row(2, 6), [0x80; 6]);
    let mut yuvs = [Plane::new(8, 3)];
    write(&samples, &format(JpegOutputFormat::Yuyv), &mut yuvs)?;
    assert_eq!(yuvs[0].row(0, 8), [0, 0x80, 1, 0x80, 2, 0x80, 2, 0x80]);
    Ok(())
}

#[test]
fn what_would_change_the_samples_is_refused_and_writes_nothing() {
    let (y, cb, cr) = (plane(0, 4, 4, 4), plane(0, 4, 4, 4), plane(0, 4, 4, 4));
    let samples = DecodedSamples {
        subsampling: JpegSubsampling::Yuv444,
        luma: view(&y, 0, 4, 1),
        chroma: Some([view(&cb, 0, 4, 1), view(&cr, 0, 4, 1)]),
    };
    let mut planes = [Plane::new(4, 4), Plane::new(4, 2)];
    let into = |format| request(4, 4, JpegSubsampling::Yuv444, whole(4, 4), format);
    assert!(matches!(
        write(&samples, &into(JpegOutputFormat::Biplanar420), &mut planes),
        Err(JpegError::UnsupportedFormat(_))
    ));
    let mut bgra = [Plane::new(16, 4)];
    assert!(matches!(
        write(&samples, &into(JpegOutputFormat::Bgra), &mut bgra),
        Err(JpegError::UnsupportedFormat(_))
    ));
    let mut halved = into(JpegOutputFormat::Biplanar444);
    halved.scale = 2;
    assert_eq!(
        write(&samples, &halved, &mut planes),
        Err(JpegError::UnsupportedScale(2))
    );
    assert!(planes.iter().all(|p| p.bytes.iter().all(|&b| b == 0xee)));
}

#[test]
fn a_short_source_or_destination_is_a_malfunction() {
    let bytes = nv12(6, 4, 8);
    let short = nv12_samples(&bytes[..40], 4, 8);
    let request = request(
        6,
        4,
        JpegSubsampling::Yuv420,
        whole(6, 4),
        JpegOutputFormat::Biplanar420,
    );
    let mut planes = [Plane::new(6, 4), Plane::new(6, 2)];
    assert!(matches!(
        write(&short, &request, &mut planes),
        Err(JpegError::Malfunction(_))
    ));
    let samples = nv12_samples(&bytes, 4, 8);
    let mut narrow = [Plane::new(5, 4), Plane::new(6, 2)];
    assert!(matches!(
        write(&samples, &request, &mut narrow),
        Err(JpegError::Malfunction(_))
    ));
}

#[test]
fn bgra_goes_in_row_for_row_and_only_its_corner() -> Result<(), JpegError> {
    // A 5x3 picture of BGRA pixels at a pitch of 24, from a driver's
    // image of 6x4, into a destination of 8 pixels a row.
    let source = plane(0, 24, 4, 24);
    let pixels = view(&source, 0, 24, 4);
    let mut planes = [Plane::new(32, 4)];
    let mut destinations: Vec<PlaneDestination<'_>> =
        planes.iter_mut().map(Plane::destination).collect();
    write_bgra(&pixels, (5, 3), &mut destinations)?;
    for row in 0..3 {
        assert_eq!(planes[0].row(row, 20), &source[row * 24..row * 24 + 20]);
        assert!(
            planes[0].bytes[row * 32 + 20..row * 32 + 32]
                .iter()
                .all(|&b| b == 0xee)
        );
    }
    assert!(planes[0].row(3, 32).iter().all(|&b| b == 0xee));
    // Rows longer than the source's pitch, a source short of the rows,
    // and pixels not four bytes each.
    let mut destinations: Vec<PlaneDestination<'_>> =
        planes.iter_mut().map(Plane::destination).collect();
    assert!(matches!(
        write_bgra(&pixels, (7, 3), &mut destinations),
        Err(JpegError::Malfunction(_))
    ));
    assert!(matches!(
        write_bgra(&view(&source, 24, 24, 4), (5, 4), &mut destinations),
        Err(JpegError::Malfunction(_))
    ));
    assert!(matches!(
        write_bgra(&view(&source, 0, 24, 3), (5, 3), &mut destinations),
        Err(JpegError::Malfunction(_))
    ));
    Ok(())
}
