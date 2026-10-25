//! The pixel formats this host's scaler converts between: the CoreVideo
//! formats of the guest's IOSurfaces, as their layouts name them, and the
//! sample layout of each.

use crate::fourcc::FourCc;
use crate::surfaces::SurfaceLayout;

/// A pixel format the scaler reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalerFormat {
    /// 8-bit 4:2:0, Y then interleaved CbCr, video range: `420v`.
    Nv12VideoRange,
    /// The same, full range: `420f`.
    Nv12FullRange,
    /// 10-bit 4:2:0 in the top bits of 16-bit little-endian samples, Y then
    /// interleaved CbCr, video range: `x420`.
    P010VideoRange,
    /// The same, full range: `xf20`.
    P010FullRange,
    /// 8-bit 4:2:2 in one plane, Y0 Cb Y1 Cr, video range: `yuvs`.
    Yuyv,
    /// 8-bit 4:2:2 in one plane, Cb Y0 Cr Y1, video range: `2vuy`.
    Uyvy,
    /// 8-bit RGB with alpha, bytes B G R A: `BGRA`.
    Bgra,
    /// 8-bit RGB with alpha, bytes A R G B: CoreVideo's
    /// `kCVPixelFormatType_32ARGB`, the code 0x20.
    Argb,
}

/// How a format's samples are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    /// A luma plane, then a plane of interleaved Cb and Cr at half the width
    /// and half the height.
    Biplanar420,
    /// One plane of 4:2:2 pairs, four bytes for two pixels; the byte
    /// offsets of Y0, Cb, Y1 and Cr in each.
    Packed422([usize; 4]),
    /// One plane of four bytes a pixel; the byte offsets of R, G, B and A.
    Rgb32([usize; 4]),
}

impl ScalerFormat {
    /// Every format, in the order the scaler lists them.
    pub const ALL: [ScalerFormat; 8] = [
        Self::Nv12VideoRange,
        Self::Nv12FullRange,
        Self::P010VideoRange,
        Self::P010FullRange,
        Self::Yuyv,
        Self::Uyvy,
        Self::Bgra,
        Self::Argb,
    ];

    /// The CoreVideo pixel format type.
    pub const fn fourcc(self) -> FourCc {
        match self {
            Self::Nv12VideoRange => FourCc::new(*b"420v"),
            Self::Nv12FullRange => FourCc::new(*b"420f"),
            Self::P010VideoRange => FourCc::new(*b"x420"),
            Self::P010FullRange => FourCc::new(*b"xf20"),
            Self::Yuyv => FourCc::new(*b"yuvs"),
            Self::Uyvy => FourCc::new(*b"2vuy"),
            Self::Bgra => FourCc::new(*b"BGRA"),
            Self::Argb => FourCc(0x20),
        }
    }

    /// The format a CoreVideo pixel format type names, if the scaler knows
    /// it.
    pub fn from_fourcc(format: FourCc) -> Option<Self> {
        Self::ALL.into_iter().find(|known| known.fourcc() == format)
    }

    /// How its samples are laid out.
    pub const fn packing(self) -> Packing {
        match self {
            Self::Nv12VideoRange
            | Self::Nv12FullRange
            | Self::P010VideoRange
            | Self::P010FullRange => Packing::Biplanar420,
            Self::Yuyv => Packing::Packed422([0, 1, 2, 3]),
            Self::Uyvy => Packing::Packed422([1, 0, 3, 2]),
            Self::Bgra => Packing::Rgb32([2, 1, 0, 3]),
            Self::Argb => Packing::Rgb32([1, 2, 3, 0]),
        }
    }

    /// Whether it is Y'CbCr, as against RGB.
    pub const fn is_ycbcr(self) -> bool {
        !matches!(self.packing(), Packing::Rgb32(_))
    }

    /// Whether its code values span the whole range, as against video
    /// range (luma 16..235 and chroma 16..240 at 8 bits). RGB is full
    /// range.
    pub const fn is_full_range(self) -> bool {
        matches!(
            self,
            Self::Nv12FullRange | Self::P010FullRange | Self::Bgra | Self::Argb
        )
    }

    /// The bits of each sample: 8, or 10.
    pub const fn bits(self) -> u32 {
        match self {
            Self::P010VideoRange | Self::P010FullRange => 10,
            _ => 8,
        }
    }

    /// The bytes one stored sample takes in its plane: 1, or 2 for 10-bit
    /// samples.
    pub const fn bytes_per_sample(self) -> usize {
        if self.bits() > 8 { 2 } else { 1 }
    }

    /// How many planes it has.
    pub const fn plane_count(self) -> usize {
        match self.packing() {
            Packing::Biplanar420 => 2,
            Packing::Packed422(_) | Packing::Rgb32(_) => 1,
        }
    }

    /// The chroma's subsampling, horizontally and vertically: (2, 2) for
    /// 4:2:0, (2, 1) for 4:2:2, (1, 1) for RGB.
    pub const fn subsampling(self) -> (u32, u32) {
        match self.packing() {
            Packing::Biplanar420 => (2, 2),
            Packing::Packed422(_) => (2, 1),
            Packing::Rgb32(_) => (1, 1),
        }
    }
}

/// Whether Apple's scaler refuses to convert `from` into `to`, with
/// kIOReturnUnsupported, which a transform between them here answers too:
/// whenever either is 2vuy or 32ARGB, 28 of the 64 pairs. Measured on the
/// M3 Max's scaler (macOS 27.0.1, 26A434), 2026-10-25, over every pair of
/// the eight formats through TransformSurface, TransferSurface and
/// TransferSurface with FixUpscaling, each at a copy, a scale up and a
/// scale down: the same answer each time, success for every other pair
/// (docs/video/scaler.md, "Apple's scaler, measured").
pub const fn apple_refuses(from: ScalerFormat, to: ScalerFormat) -> bool {
    matches!(from, ScalerFormat::Uyvy | ScalerFormat::Argb)
        || matches!(to, ScalerFormat::Uyvy | ScalerFormat::Argb)
}

/// One plane of a surface as the scaler reads and writes it: where it
/// starts, its row pitch, and the bytes of each row its samples fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneSpan {
    /// Where the plane starts in the surface's memory.
    pub offset: usize,
    /// The bytes from one row to the next.
    pub bytes_per_row: usize,
    /// The rows it has.
    pub rows: usize,
    /// The bytes of a row that hold samples.
    pub row_bytes: usize,
}

/// The planes of a surface in `format`, from its layout, each checked to
/// hold its samples: `None` where the layout is not that format's, or a
/// plane's rows are too short for its samples.
pub fn plane_spans(format: ScalerFormat, layout: &SurfaceLayout) -> Option<Vec<PlaneSpan>> {
    let width = usize::try_from(layout.width).ok()?;
    let height = usize::try_from(layout.height).ok()?;
    let spans = match format.packing() {
        Packing::Biplanar420 => {
            let [luma, chroma] = layout.planes.as_slice() else {
                return None;
            };
            let sample = format.bytes_per_sample();
            vec![
                PlaneSpan {
                    offset: luma.offset,
                    bytes_per_row: luma.bytes_per_row,
                    rows: height,
                    row_bytes: width.checked_mul(sample)?,
                },
                PlaneSpan {
                    offset: chroma.offset,
                    bytes_per_row: chroma.bytes_per_row,
                    rows: height.div_ceil(2),
                    row_bytes: width.div_ceil(2).checked_mul(2 * sample)?,
                },
            ]
        }
        Packing::Packed422(_) | Packing::Rgb32(_) => {
            let (offset, bytes_per_row) = match layout.planes.as_slice() {
                [] => (0, layout.bytes_per_row),
                [plane] => (plane.offset, plane.bytes_per_row),
                _ => return None,
            };
            let row_bytes = match format.packing() {
                Packing::Packed422(_) => width.div_ceil(2).checked_mul(4)?,
                _ => width.checked_mul(4)?,
            };
            vec![PlaneSpan {
                offset,
                bytes_per_row,
                rows: height,
                row_bytes,
            }]
        }
    };
    spans
        .iter()
        .all(|s| s.row_bytes <= s.bytes_per_row)
        .then_some(spans)
}

impl PlaneSpan {
    /// The bytes from the plane's start to the end of its last row's
    /// samples.
    pub fn extent(&self) -> Option<usize> {
        match self.rows {
            0 => Some(0),
            rows => (rows - 1)
                .checked_mul(self.bytes_per_row)?
                .checked_add(self.row_bytes),
        }
    }

    /// Whether the plane lies within `length` bytes of memory.
    pub fn fits(&self, length: usize) -> bool {
        self.extent()
            .and_then(|extent| self.offset.checked_add(extent))
            .is_some_and(|end| end <= length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surfaces::PlaneLayout;

    #[test]
    fn each_format_is_its_fourcc() {
        for format in ScalerFormat::ALL {
            assert_eq!(ScalerFormat::from_fourcc(format.fourcc()), Some(format));
        }
        assert_eq!(ScalerFormat::from_fourcc(FourCc::new(*b"v210")), None);
    }

    #[test]
    fn a_biplanar_surface_needs_its_two_planes() {
        let nv12 = SurfaceLayout {
            pixel_format: FourCc::new(*b"420v"),
            width: 5,
            height: 3,
            planes: vec![
                PlaneLayout {
                    offset: 0,
                    bytes_per_row: 64,
                    width: 5,
                    height: 3,
                },
                PlaneLayout {
                    offset: 192,
                    bytes_per_row: 64,
                    width: 3,
                    height: 2,
                },
            ],
            bytes_per_row: 0,
        };
        let spans = plane_spans(ScalerFormat::Nv12VideoRange, &nv12);
        assert_eq!(
            spans.as_ref().map(|s| (s[1].rows, s[1].row_bytes)),
            Some((2, 6))
        );
        assert_eq!(spans.map(|s| s[1].fits(192 + 64 + 6)), Some(true));
        let bgra = SurfaceLayout {
            planes: Vec::new(),
            bytes_per_row: 16,
            ..nv12.clone()
        };
        assert_eq!(plane_spans(ScalerFormat::Nv12VideoRange, &bgra), None);
        assert_eq!(
            plane_spans(ScalerFormat::Bgra, &bgra),
            None,
            "5 pixels need 20 bytes"
        );
    }
}
