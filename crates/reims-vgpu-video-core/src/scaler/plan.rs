//! A transform resolved against its two surfaces: what a scaler backend
//! does, in whole terms. Each rule here is what Apple's scaler was
//! measured to do, or ours where it is not known, and says so.

use crate::num::ToF64;
use crate::scaler::format::{PlaneSpan, ScalerFormat, apple_refuses, plane_spans};
use crate::scaler::io_return::{self, IoReturn};
use crate::scaler::request::{FIXED_ONE, FixedRect, Rect, Transform};
use crate::surfaces::SurfaceLayout;

/// How the source window is turned on its way to the destination window:
/// the `Transform` option's bits 0 to 2. Measured on Apple's scaler: bit 2
/// rotates 90 degrees clockwise, then bit 0 flips the result vertically
/// and bit 1 horizontally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Orientation {
    /// As it is (0).
    Identity,
    /// Upside down (1).
    FlipVertical,
    /// Left to right (2).
    FlipHorizontal,
    /// Turned 180 degrees (3).
    Rotate180,
    /// Turned 90 degrees clockwise (4).
    Rotate90,
    /// Turned 90 degrees clockwise, then upside down (5): the source's
    /// anti-transpose.
    Rotate90FlipVertical,
    /// Turned 90 degrees clockwise, then left to right (6): the source's
    /// transpose.
    Rotate90FlipHorizontal,
    /// Turned 90 degrees counter-clockwise (7).
    Rotate270,
}

impl Orientation {
    /// The orientation of the `Transform` option's bits. Bits 3 to 5 are
    /// ignored, as Apple's scaler ignored bit 3 (measured).
    pub const fn from_bits(bits: u32) -> Orientation {
        match bits & 7 {
            0 => Self::Identity,
            1 => Self::FlipVertical,
            2 => Self::FlipHorizontal,
            3 => Self::Rotate180,
            4 => Self::Rotate90,
            5 => Self::Rotate90FlipVertical,
            6 => Self::Rotate90FlipHorizontal,
            _ => Self::Rotate270,
        }
    }

    /// Whether it swaps width and height.
    pub const fn rotates(self) -> bool {
        matches!(
            self,
            Self::Rotate90
                | Self::Rotate90FlipVertical
                | Self::Rotate90FlipHorizontal
                | Self::Rotate270
        )
    }

    /// Whether it turns 90 degrees clockwise, flips vertically and flips
    /// horizontally, in that order.
    pub const fn parts(self) -> (bool, bool, bool) {
        match self {
            Self::Identity => (false, false, false),
            Self::FlipVertical => (false, true, false),
            Self::FlipHorizontal => (false, false, true),
            Self::Rotate180 => (false, true, true),
            Self::Rotate90 => (true, false, false),
            Self::Rotate90FlipVertical => (true, true, false),
            Self::Rotate90FlipHorizontal => (true, false, true),
            Self::Rotate270 => (true, true, true),
        }
    }

    /// The point of a `width` by `height` source that lands at `(x, y)` of
    /// the oriented picture, in continuous coordinates (pixel centres at
    /// half units).
    pub fn source_point(self, x: f64, y: f64, width: f64, height: f64) -> (f64, f64) {
        let (rotate, flip_v, flip_h) = self.parts();
        let (ow, oh) = if rotate {
            (height, width)
        } else {
            (width, height)
        };
        let x = if flip_h { ow - x } else { x };
        let y = if flip_v { oh - y } else { y };
        if rotate {
            // Clockwise: the oriented picture's column x is the source's
            // row height - x, its row y the source's column y.
            (y, height - x)
        } else {
            (x, y)
        }
    }
}

/// How samples are taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Filter {
    /// The scaler's own filter.
    Smooth,
    /// The nearest sample (`NearestFilter`).
    Nearest,
}

/// The Y'CbCr matrix between Y'CbCr and RGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Matrix {
    /// ITU-R BT.601.
    Bt601,
    /// ITU-R BT.709.
    Bt709,
}

impl Matrix {
    /// Its luma weights of red and blue.
    pub const fn weights(self) -> (f64, f64) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
        }
    }
}

/// Where a subsampled chroma sample sits along one axis among the luma
/// pixels it covers. Ours, for what Apple's scaler measured to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Site {
    /// On the first pixel: the left column, or the top row.
    First,
    /// Between the two.
    Between,
    /// On the second pixel: the bottom row.
    Second,
}

/// Where a 4:2:0 or 4:2:2 surface's chroma samples sit, across and down
/// (down only for 4:2:0). Ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Siting {
    /// Across.
    pub horizontal: Site,
    /// Down.
    pub vertical: Site,
}

impl Siting {
    /// Where Apple's scaler takes a Y'CbCr source's chroma to sit, always,
    /// and where it writes a destination's that names none: on the first
    /// column and row. Measured on the M3 Max's AppleM2ScalerCSCDriver
    /// (macOS 27.0.1, 2026-10-24): a source's chroma is interpolated
    /// between from its first pixels whatever its own siting says, and a
    /// destination's sample is its first pixel.
    pub const APPLE: Siting = Siting {
        horizontal: Site::First,
        vertical: Site::First,
    };

    /// Between the pixels both ways, as JFIF sites chroma, and as the
    /// reference makes a picture's chroma when it draws one.
    pub const CENTRED: Siting = Siting {
        horizontal: Site::Between,
        vertical: Site::Between,
    };

    /// The siting the transform's flag bits 15 to 17 name for its
    /// destination, as IOSurfaceAccelerator sets them from the
    /// destination's chroma location: 1 left, 2 centre, 3 top left, 4 top,
    /// 5 bottom left, 6 bottom; 0, none, is [`Siting::APPLE`]. Apple's
    /// scaler measured to write each so, a sample on the pixel named or
    /// the mean of the two (above). 7, which the framework never sends, is
    /// taken as none: ours.
    pub const fn of_cosite(bits: u32) -> Siting {
        let (horizontal, vertical) = match bits {
            1 => (Site::First, Site::Between),
            2 => (Site::Between, Site::Between),
            4 => (Site::Between, Site::First),
            5 => (Site::First, Site::Second),
            6 => (Site::Between, Site::Second),
            _ => (Site::First, Site::First),
        };
        Siting {
            horizontal,
            vertical,
        }
    }
}

/// Where a scale takes each destination pixel from along one axis of the
/// destination, in the source window as turned. Ours, for what Apple's
/// scaler was measured to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Alignment {
    /// The windows' pixel centres kept: destination pixel `d` of a window
    /// `D` long takes the source window's `(d + 0.5) W / D - 0.5`, with
    /// pixel `i`'s centre at `i`. Apple's scaler enlarges so with
    /// `FixUpscaling` (flag bit 12).
    Centres,
    /// The windows' first pixels on each other: `d W / D`. Apple's scaler
    /// shrinks so always, and enlarges so without `FixUpscaling`.
    FirstPixels,
}

impl Alignment {
    /// How Apple's scaler takes an axis that `enlarges` (its destination
    /// window longer than the turned source window's) with `FixUpscaling`
    /// set or not. Measured on the M3 Max's AppleM2ScalerCSCDriver (macOS
    /// 27.0.1, 2026-10-24), for 420v and BGRA, from 1.1 to 8 times up and 1.5
    /// to 4 times down, odd sizes and quarter turns among them: every
    /// enlargement through TransferSurface took `d W / D` (within 0.05 of a
    /// source pixel), and through TransferSurface with the FixUpscaling
    /// option and through TransformSurface `(d + 0.5) W / D - 0.5` (within
    /// 0.05, but for 2x, within 0.09); every reduction through all three took
    /// `d W / D`.
    pub const fn of(enlarges: bool, fix_upscaling: bool) -> Alignment {
        if enlarges && fix_upscaling {
            Self::Centres
        } else {
            Self::FirstPixels
        }
    }

    /// The source position, in source pixels from the centre of the
    /// window's first, that destination pixel `d` of its window takes,
    /// `ratio` being the source window's side over the destination's.
    pub fn source_index(self, d: f64, ratio: f64) -> f64 {
        match self {
            Self::Centres => (d + 0.5) * ratio - 0.5,
            Self::FirstPixels => d * ratio,
        }
    }

    /// [`Alignment::source_index`] for the chroma of a Y'CbCr source whose
    /// chroma samples cover `subsampling` pixels along the axis: where it
    /// keeps centres, the centre it keeps lies `c = 1 / (2 subsampling)` of
    /// a pixel in, `(d + c) W / D - c`, half the luma's offset where the
    /// source subsamples and the luma's where it does not; on first pixels,
    /// the luma's place.
    ///
    /// Measured on the M3 Max's AppleM2ScalerCSCDriver (macOS 27.0.1,
    /// 2026-10-25) with x420, 420v and yuvs sources of chroma impulses,
    /// enlarged through TransformSurface 1.03 to 8 times across and down
    /// into x420, 420v, yuvs and BGRA, quarter turns, a window and a
    /// destination window among them. At 2, 2.5, 3, 4 and 8 times, the
    /// impulses' centroids in x420 fitted maps whose offsets were within
    /// 0.008 of a chroma sample of this. Every picture matched the
    /// reference within a code with it, where the luma's offset left x420
    /// chroma up to 18 codes off, 420v's 6 and BGRA's 8, and half of it a
    /// yuvs source's chroma down 7 off.
    pub fn chroma_source_index(self, d: f64, ratio: f64, subsampling: u32) -> f64 {
        match self {
            Self::Centres => {
                let c = 0.5 / f64::from(subsampling.max(1));
                (d + c) * ratio - c
            }
            Self::FirstPixels => d * ratio,
        }
    }

    /// How far, in source pixels, a scaler that keeps pixel centres has to
    /// move its source window for its samples to land where this alignment
    /// puts them: `(1 - ratio) / 2` for [`Alignment::FirstPixels`], so a
    /// quarter of a source pixel on for a doubling and half of one back
    /// for a halving.
    pub fn shift(self, ratio: f64) -> f64 {
        match self {
            Self::Centres => 0.0,
            Self::FirstPixels => (1.0 - ratio) / 2.0,
        }
    }
}

/// A transform in whole terms.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// The source's format.
    pub source_format: FormatOf,
    /// The destination's format.
    pub destination_format: FormatOf,
    /// The source window, 16.16, within the source.
    pub source_rect: FixedRect,
    /// The destination window, within the destination; the pixels outside
    /// it are left as they are.
    pub destination_rect: Rect,
    /// How the source window is turned.
    pub orientation: Orientation,
    /// How it is sampled.
    pub filter: Filter,
    /// The matrix between Y'CbCr and RGB, where the two formats differ in
    /// that.
    pub matrix: Matrix,
    /// Where the destination's chroma is written to sit, where it has
    /// subsampled chroma. A Y'CbCr source's is always read as
    /// [`Siting::APPLE`], as Apple's scaler reads it.
    pub destination_siting: Siting,
    /// Where each destination pixel is taken from, across and down the
    /// destination ([`Alignment::of`]).
    pub alignment: [Alignment; 2],
}

/// A surface's format, size and planes as a plan reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatOf {
    /// Its format.
    pub format: ScalerFormat,
    /// Its width.
    pub width: u32,
    /// Its height.
    pub height: u32,
    /// Its planes.
    pub planes: Vec<PlaneSpan>,
}

impl FormatOf {
    /// The format and planes of a surface: kIOReturnUnsupported for a
    /// format the scaler does not know, kIOReturnBadArgument for a layout
    /// that does not hold its format's samples.
    pub fn of(layout: &SurfaceLayout) -> Result<FormatOf, IoReturn> {
        let format =
            ScalerFormat::from_fourcc(layout.pixel_format).ok_or(io_return::UNSUPPORTED)?;
        let planes = plane_spans(format, layout).ok_or(io_return::BAD_ARGUMENT)?;
        if layout.width == 0 || layout.height == 0 {
            return Err(io_return::BAD_ARGUMENT);
        }
        Ok(FormatOf {
            format,
            width: layout.width,
            height: layout.height,
            planes,
        })
    }

    /// Whether every plane fits in `length` bytes of the surface's memory.
    pub fn fits(&self, length: usize) -> bool {
        self.planes.iter().all(|p| p.fits(length))
    }
}

/// The first option of `transform` that changes its output in a way no
/// backend here does, by its name in IOSurfaceAccelerator: None when there
/// is none. Ours.
///
/// The rest are hints a backend may ignore and still give the output the
/// caller asked for: the filter's tuning (`PixelAveraging`,
/// `EdgeMirroring`, `DisablePseudoLinearScaling`, `DirectionalScaling`,
/// `SymmetricScaling`, `DitherControl`), the scheduling
/// and the memory's (`LockInScaler`, `SkipAsyncCallback`, `UnwireSurface`,
/// `NoPowerEarlyBail`, `FirmwareTimeoutInject`, `ReadOnly`,
/// `NoCoalesceAsyncIOFenceWrites`, `DestinationDisplayBound`), and the
/// side buffers' own, which come only with `HDREnable`.
pub fn refused_option(transform: &Transform) -> Option<&'static str> {
    let flags = transform.flags;
    [
        (flags.alpha_override(), "AlphaOverride"),
        (flags.border_fill(), "BorderFill"),
        (flags.histogram(), "HistogramBinMode"),
        (flags.zero_fill(), "ZeroFill"),
        (flags.color_remap_mode() != 0, "ColorRemapMode"),
        (flags.hdr(), "HDREnable"),
        (flags.enhance(), "EnhanceEnable"),
        (flags.write_only_mode(), "WriteOnlyMode"),
        (flags.film_grain(), "FilmGrainEnable"),
        (flags.border_fill_odd_size(), "BorderFillOddSizeYCbCrEnable"),
    ]
    .into_iter()
    .find_map(|(set, option)| set.then_some(option))
}

/// A source window along one axis of a surface `extent` pixels long, `at`
/// and `length` in 16.16, as Apple's scaler takes it into `to` destination
/// pixels. One that starts on a pixel spans its length's whole pixels. One
/// that starts within a pixel spans from that pixel to the one its end is
/// in, the pixel past it where it ends on one; and where it ends on the
/// surface's last, its length and one pixel more. Where those are `to`
/// whole pixels, it copies them, from the first; otherwise it scales them
/// from where the window starts, its fraction kept.
///
/// Measured on the M3 Max's AppleM2ScalerCSCDriver (macOS 27.0.1,
/// 2026-10-25) with BGRA windows of a 128x96 picture: (10.5, 6.25) 64x48,
/// (0.5, 0.5) 64x48, (10.125, 6.0625) and (10.9, 6.1) 64x48 into 65x49,
/// (10.75, 6.5) 63x47 into 64x48 and (10, 6) 63.5x47.25 into 63x47 copied
/// so exactly; the red and green of (10.5, 6.25) 64x48 into 64x48 and
/// 32x24, (10.25, 6.75) 63.5x47.5 and (10, 6) 63.5x47.25 into 64x48,
/// (3.75, 5.5) 60x45 into 40x30 and (20.25, 30.75) 40x30 into 100x75 matched
/// the reference with the window so within a code on 97% to 100% of
/// samples, and within 3 on the rest, where the window as given was up to
/// 217 codes off. Red impulses through windows that end on a pixel (the
/// same day) gave each window's map from the impulses' places: (10.5, 0)
/// 63.5 wide into 64, 80 and 96, (0.5, 0) 63.5 into 64 and (20.25, 0)
/// 43.75 into 44 and 50 scaled 65, 65 and 45 pixels from where they start,
/// within 0.05 of a pixel; and (0.5, 0) 127.5 wide of 128 into 64, 118,
/// 127, 128, 129 and 256, (0.25, 0) 127.75 and (0.75, 0) 127.25 into 128,
/// (1.5, 0) 126.5 into 128 and (10.5, 0) 117.5 into 118 scaled their length
/// and one more, within 0.1.
pub fn touched(at: u64, length: u64, extent: u32, to: u32) -> (u64, u64) {
    let first = at / FIXED_ONE * FIXED_ONE;
    let end = at + length;
    let whole = if at == first {
        length / FIXED_ONE * FIXED_ONE
    } else if end == u64::from(extent) * FIXED_ONE {
        length + FIXED_ONE
    } else {
        end / FIXED_ONE * FIXED_ONE + FIXED_ONE - first
    };
    if whole == u64::from(to) * FIXED_ONE {
        (first, whole)
    } else {
        (at, whole)
    }
}

/// A window along one axis, `at` and `length` in units `one` to a pixel,
/// as Apple's scaler takes it where the surface keeps one chroma sample for
/// `subsampling` pixels along the axis. For 2 it starts on the even pixel
/// at or before its first, its fraction dropped, and spans its length
/// rounded down to an even number of pixels, so an odd surface's last
/// column or row is left as it was. A window under two pixels is kept as
/// it is: ours, as Apple's scaler takes no side under 4.
///
/// Measured on the M3 Max's AppleM2ScalerCSCDriver (macOS 27.0.1,
/// 2026-10-25), copies of a 128x96 picture in 420v and yuvs: destination
/// windows at (13, 9), (12, 8) and (13, 8), 63x47 and 64x48, and source
/// windows the same, each wrote exactly what the window so rounded copies,
/// and nothing outside it. A whole 65x49 420v copied its 64x48 and left the
/// last column and row, and scaled 64x48 into 130x98. A 420v source window
/// (10.5, 6.5) 64x48 copied (10, 6) 64x48, and (11, 7) 63.5x47.5 into 64x48
/// scaled (10, 6) 62x46 as the reference does within a code. yuvs rounded
/// across alone, and a BGRA end of each case took its window as given.
pub fn even_window(at: u64, length: u64, subsampling: u32, one: u64) -> (u64, u64) {
    if subsampling < 2 {
        return (at, length);
    }
    let pair = 2 * one;
    let even = length / pair * pair;
    if even == 0 {
        return (at, length);
    }
    (at / pair * pair, even)
}

impl Plan {
    /// The plan of `transform` from `source` to `destination`.
    ///
    /// - A window whose width or height is 0 is the whole surface, as
    ///   IOSurfaceAccelerator's calls without rectangles leave them
    ///   (measured: such a transfer fills the destination).
    /// - A window that leaves its surface is kIOReturnBadArgument: ours,
    ///   as Apple's scaler's answer is not measured.
    /// - A source window takes whole pixels ([`touched`]): a copy where they
    ///   are the destination window's, a scale from the window's start,
    ///   its fraction kept, where not, as Apple's scaler does.
    /// - Along an axis a Y'CbCr surface subsamples chroma on, its window
    ///   starts on an even pixel and spans an even number of them, each
    ///   rounded down ([`even_window`]), as Apple's scaler takes it.
    /// - The matrix is BT.601, the one Apple's scaler uses for a surface
    ///   with no matrix attached (measured), which every surface this host
    ///   sees is: the guest's mapper sends no colour attachments.
    /// - The destination's chroma siting is the one flag bits 15 to 17
    ///   name ([`Siting::of_cosite`]).
    /// - Each axis keeps pixel centres where it enlarges with
    ///   `FixUpscaling` (bit 12), and puts the windows' first pixels on
    ///   each other otherwise ([`Alignment::of`]), as Apple's scaler does.
    /// - A pair of formats Apple's scaler does not convert between
    ///   ([`apple_refuses`]) is kIOReturnUnsupported, as Apple's answers.
    /// - An option that changes the output in a way this host does not do
    ///   ([`refused_option`]) is kIOReturnUnsupported, as Apple's scaler
    ///   answers a conversion it does not do (measured). Ours: what Apple's
    ///   does with an option its hardware lacks is not measured.
    pub fn new(
        transform: &Transform,
        source: &SurfaceLayout,
        destination: &SurfaceLayout,
    ) -> Result<Plan, IoReturn> {
        Plan::resolve(transform, source, destination, true)
    }

    /// [`Plan::new`], but for a pair of formats Apple's scaler refuses
    /// ([`apple_refuses`]), which this plans as any other: for the tests of
    /// the reference and of the backends, which do every pair.
    pub fn of_any_pair(
        transform: &Transform,
        source: &SurfaceLayout,
        destination: &SurfaceLayout,
    ) -> Result<Plan, IoReturn> {
        Plan::resolve(transform, source, destination, false)
    }

    fn resolve(
        transform: &Transform,
        source: &SurfaceLayout,
        destination: &SurfaceLayout,
        refuse_pairs: bool,
    ) -> Result<Plan, IoReturn> {
        if let Some(option) = refused_option(transform) {
            log::warn!("scaler: refused, as this host does not do {option}");
            return Err(io_return::UNSUPPORTED);
        }
        let source_format = FormatOf::of(source)?;
        let destination_format = FormatOf::of(destination)?;
        if refuse_pairs && apple_refuses(source_format.format, destination_format.format) {
            log::warn!(
                "scaler: refused, as Apple's scaler does not convert {} into {}",
                source_format.format.fourcc(),
                destination_format.format.fourcc()
            );
            return Err(io_return::UNSUPPORTED);
        }
        let mut source_rect = transform.source_rect;
        if source_rect.width == 0 || source_rect.height == 0 {
            source_rect = FixedRect {
                x: 0,
                y: 0,
                width: u64::from(source.width) * FIXED_ONE,
                height: u64::from(source.height) * FIXED_ONE,
            };
        }
        let within = |at: u64, length: u64, limit: u32| {
            at.checked_add(length)
                .is_some_and(|end| end <= u64::from(limit) * FIXED_ONE)
        };
        if !within(source_rect.x, source_rect.width, source.width)
            || !within(source_rect.y, source_rect.height, source.height)
        {
            return Err(io_return::BAD_ARGUMENT);
        }
        let mut destination_rect = transform.destination_rect;
        if destination_rect.width == 0 || destination_rect.height == 0 {
            destination_rect = Rect {
                x: 0,
                y: 0,
                width: destination.width,
                height: destination.height,
            };
        }
        let inside = |at: u32, length: u32, limit: u32| {
            at.checked_add(length).is_some_and(|end| end <= limit)
        };
        if !inside(
            destination_rect.x,
            destination_rect.width,
            destination.width,
        ) || !inside(
            destination_rect.y,
            destination_rect.height,
            destination.height,
        ) {
            return Err(io_return::BAD_ARGUMENT);
        }
        // The windows as Apple's scaler takes them: the destination's in
        // even pairs where it subsamples chroma, then the source's whole
        // pixels ([`touched`]), in even pairs where it subsamples.
        let (dx, dy) = destination_format.format.subsampling();
        let even = |at: u32, length: u32, subsampling: u32| {
            let (a, l) = even_window(at.into(), length.into(), subsampling, 1);
            // Each is no more than what it came from, so it fits.
            (
                u32::try_from(a).unwrap_or(at),
                u32::try_from(l).unwrap_or(length),
            )
        };
        let (x, width) = even(destination_rect.x, destination_rect.width, dx);
        let (y, height) = even(destination_rect.y, destination_rect.height, dy);
        let destination_rect = Rect {
            x,
            y,
            width,
            height,
        };
        let orientation = Orientation::from_bits(transform.flags.symmetric_transform());
        let (to_x, to_y) = if orientation.rotates() {
            (destination_rect.height, destination_rect.width)
        } else {
            (destination_rect.width, destination_rect.height)
        };
        let (sx, sy) = source_format.format.subsampling();
        let (x, width) = touched(source_rect.x, source_rect.width, source.width, to_x);
        let (x, width) = even_window(x, width, sx, FIXED_ONE);
        let (y, height) = touched(source_rect.y, source_rect.height, source.height, to_y);
        let (y, height) = even_window(y, height, sy, FIXED_ONE);
        let source_rect = FixedRect {
            x,
            y,
            width,
            height,
        };
        let mut plan = Plan {
            source_format,
            destination_format,
            source_rect,
            destination_rect,
            orientation,
            filter: if transform.flags.nearest_filter() {
                Filter::Nearest
            } else {
                Filter::Smooth
            },
            matrix: Matrix::Bt601,
            destination_siting: Siting::of_cosite(transform.flags.cosite()),
            alignment: [Alignment::FirstPixels; 2],
        };
        plan.alignment = plan.alignment_with(transform.flags.fix_upscaling());
        Ok(plan)
    }

    /// Each axis's alignment, across and down the destination, were
    /// `FixUpscaling` set as `fix` says ([`Alignment::of`]).
    pub fn alignment_with(&self, fix: bool) -> [Alignment; 2] {
        let (across, down) = if self.orientation.rotates() {
            (self.source_rect.height, self.source_rect.width)
        } else {
            (self.source_rect.width, self.source_rect.height)
        };
        let enlarges =
            |from: u64, to: u32| u128::from(to) * u128::from(FIXED_ONE) > u128::from(from);
        [
            Alignment::of(enlarges(across, self.destination_rect.width), fix),
            Alignment::of(enlarges(down, self.destination_rect.height), fix),
        ]
    }

    /// The ratio of the turned source window's side to the destination
    /// window's, across and down the destination.
    pub fn ratios(&self) -> (f64, f64) {
        let (across, down) = if self.orientation.rotates() {
            (self.source_rect.height, self.source_rect.width)
        } else {
            (self.source_rect.width, self.source_rect.height)
        };
        let fixed = |v: u64| v.to_f64() / FIXED_ONE.to_f64();
        (
            fixed(across) / f64::from(self.destination_rect.width),
            fixed(down) / f64::from(self.destination_rect.height),
        )
    }

    /// How far a scaler that keeps pixel centres moves the turned source
    /// window, across and down the destination, in source pixels, to
    /// sample as the plan's alignment does ([`Alignment::shift`]).
    pub fn shift(&self) -> (f64, f64) {
        self.shift_from([Alignment::Centres; 2])
    }

    /// How far a scaler that samples as `own` says moves the turned source
    /// window, across and down the destination, to sample as the plan's
    /// alignment does.
    pub fn shift_from(&self, own: [Alignment; 2]) -> (f64, f64) {
        let (rx, ry) = self.ratios();
        (
            self.alignment[0].shift(rx) - own[0].shift(rx),
            self.alignment[1].shift(ry) - own[1].shift(ry),
        )
    }

    /// The first of the capabilities' ratios the transform goes past, if
    /// any: `max_scale` is `MaxHUpscale`, `MaxHDownscale`, `MaxVUpscale`
    /// and `MaxVDownscale` ([`Capabilities::max_scale`]). The ratio is the
    /// destination window's side over the source window's, taken after the
    /// orientation, so a quarter turn measures the source's height across.
    /// A ratio at its bound is within it. Apple's scaler measured so on
    /// the M3 Max: 8x across at an offered 8 transformed, and 16x either way
    /// on either axis, 8x down the frame at an offered 4, and a quarter
    /// turn 16x up across after the turn were kIOReturnBadArgument, while a
    /// quarter turn at 1x that is 16x down across before the turn
    /// transformed. A bound of 0 is no bound: ours, as no capability
    /// Apple's offers is 0.
    ///
    /// [`Capabilities::max_scale`]: crate::scaler::capabilities::Capabilities::max_scale
    pub fn ratio_past(&self, max_scale: [u32; 4]) -> Option<&'static str> {
        let (across, down) = if self.orientation.rotates() {
            (self.source_rect.height, self.source_rect.width)
        } else {
            (self.source_rect.width, self.source_rect.height)
        };
        let across_to = u64::from(self.destination_rect.width) * FIXED_ONE;
        let down_to = u64::from(self.destination_rect.height) * FIXED_ONE;
        // A side in 16.16 is below 2^48 and a bound below 2^32, so their
        // product is taken in u128.
        let past = |from: u64, to: u64, up: u32, down: u32| {
            let (from, to) = (u128::from(from), u128::from(to));
            if up != 0 && to > from * u128::from(up) {
                Some(true)
            } else if down != 0 && from > to * u128::from(down) {
                Some(false)
            } else {
                None
            }
        };
        match past(across, across_to, max_scale[0], max_scale[1]) {
            Some(true) => return Some("MaxHUpscale"),
            Some(false) => return Some("MaxHDownscale"),
            None => {}
        }
        match past(down, down_to, max_scale[2], max_scale[3]) {
            Some(true) => Some("MaxVUpscale"),
            Some(false) => Some("MaxVDownscale"),
            None => None,
        }
    }

    /// Whether the transform changes the picture's size.
    pub fn scales(&self) -> bool {
        let (w, h) = if self.orientation.rotates() {
            (self.source_rect.height, self.source_rect.width)
        } else {
            (self.source_rect.width, self.source_rect.height)
        };
        w != u64::from(self.destination_rect.width) * FIXED_ONE
            || h != u64::from(self.destination_rect.height) * FIXED_ONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fourcc::FourCc;
    use crate::scaler::request::TransformFlags;
    use crate::surfaces::PlaneLayout;

    fn bgra(width: u32, height: u32) -> SurfaceLayout {
        SurfaceLayout {
            pixel_format: FourCc::new(*b"BGRA"),
            width,
            height,
            planes: Vec::new(),
            bytes_per_row: width as usize * 4,
        }
    }

    fn ycbcr(fourcc: &[u8; 4], width: u32, height: u32) -> SurfaceLayout {
        if fourcc == b"yuvs" {
            return SurfaceLayout {
                pixel_format: FourCc::new(*fourcc),
                width,
                height,
                planes: Vec::new(),
                bytes_per_row: width.div_ceil(2) as usize * 4,
            };
        }
        let (luma, chroma) = (width as usize, width.div_ceil(2) as usize * 2);
        SurfaceLayout {
            pixel_format: FourCc::new(*fourcc),
            width,
            height,
            planes: vec![
                PlaneLayout {
                    offset: 0,
                    bytes_per_row: luma,
                    width,
                    height,
                },
                PlaneLayout {
                    offset: luma * height as usize,
                    bytes_per_row: chroma,
                    width: width.div_ceil(2),
                    height: height.div_ceil(2),
                },
            ],
            bytes_per_row: luma,
        }
    }

    #[test]
    fn fractional_source_windows_take_whole_pixels() -> Result<(), IoReturn> {
        let at = |x: u64, quarters: u64| x * FIXED_ONE + quarters * FIXED_ONE / 4;
        let window = |x, y, width, height| Transform {
            source_rect: FixedRect {
                x,
                y,
                width,
                height,
            },
            ..Transform::default()
        };
        let source = |t: &Transform, dw, dh| {
            Plan::new(t, &bgra(128, 96), &bgra(dw, dh)).map(|p| p.source_rect)
        };
        // (10.5, 6.25) 64x48 touches 65x49 from (10, 6): copied into 65x49,
        // scaled from (10.5, 6.25) into anything else.
        let t = window(at(10, 2), at(6, 1), at(64, 0), at(48, 0));
        let whole = |x, y, width, height| FixedRect {
            x,
            y,
            width,
            height,
        };
        assert_eq!(
            source(&t, 65, 49)?,
            whole(at(10, 0), at(6, 0), at(65, 0), at(49, 0))
        );
        assert_eq!(
            source(&t, 64, 48)?,
            whole(at(10, 2), at(6, 1), at(65, 0), at(49, 0))
        );
        // A window that starts on a pixel takes its length's whole pixels.
        let t = window(at(10, 0), at(6, 0), at(63, 2), at(47, 1));
        assert_eq!(
            source(&t, 63, 47)?,
            whole(at(10, 0), at(6, 0), at(63, 0), at(47, 0))
        );
        // One that starts within a pixel and ends on one inside the surface
        // takes the pixel past its end: (10.5, 6.5) 63.5x47.5 touches 65x49.
        let t = window(at(10, 2), at(6, 2), at(63, 2), at(47, 2));
        assert_eq!(
            source(&t, 65, 49)?,
            whole(at(10, 0), at(6, 0), at(65, 0), at(49, 0))
        );
        assert_eq!(
            source(&t, 64, 48)?,
            whole(at(10, 2), at(6, 2), at(65, 0), at(49, 0))
        );
        // One that ends on the surface's last pixel takes its length and one
        // pixel more, scaled from where it starts: (0.5, 0.5) 127.5x95.5 of
        // 128x96 into 128x96 is 128.5x96.5 from (0.5, 0.5).
        let t = window(at(0, 2), at(0, 2), at(127, 2), at(95, 2));
        assert_eq!(
            source(&t, 128, 96)?,
            whole(at(0, 2), at(0, 2), at(128, 2), at(96, 2))
        );
        // A quarter turn compares each with the destination's other side.
        let t = Transform {
            flags: TransformFlags(4),
            ..window(at(10, 2), at(6, 1), at(64, 0), at(48, 0))
        };
        assert_eq!(
            source(&t, 49, 65)?,
            whole(at(10, 0), at(6, 0), at(65, 0), at(49, 0))
        );
        Ok(())
    }

    #[test]
    fn ycbcr_windows_are_taken_in_even_pairs() -> Result<(), IoReturn> {
        let pixels = |x, y, width, height| FixedRect {
            x: x * FIXED_ONE,
            y: y * FIXED_ONE,
            width: width * FIXED_ONE,
            height: height * FIXED_ONE,
        };
        let rect = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        // A whole odd 420v: its even part, at both ends.
        let odd = ycbcr(b"420v", 65, 49);
        let p = Plan::new(&Transform::default(), &odd, &odd)?;
        assert_eq!(p.source_rect, pixels(0, 0, 64, 48));
        assert_eq!(p.destination_rect, rect(0, 0, 64, 48));
        // Odd windows: each start and each length down to an even one.
        let t = Transform {
            source_rect: pixels(13, 9, 63, 47),
            destination_rect: rect(13, 9, 63, 47),
            ..Transform::default()
        };
        let big = ycbcr(b"420v", 128, 96);
        let p = Plan::new(&t, &big, &big)?;
        assert_eq!(p.source_rect, pixels(12, 8, 62, 46));
        assert_eq!(p.destination_rect, rect(12, 8, 62, 46));
        // A fraction is dropped.
        let t = Transform {
            source_rect: FixedRect {
                x: 10 * FIXED_ONE + FIXED_ONE / 2,
                y: 6 * FIXED_ONE + FIXED_ONE / 2,
                width: 64 * FIXED_ONE,
                height: 48 * FIXED_ONE,
            },
            ..Transform::default()
        };
        assert_eq!(
            Plan::new(&t, &big, &ycbcr(b"420v", 64, 48))?.source_rect,
            pixels(10, 6, 64, 48)
        );
        // yuvs pairs across alone, and BGRA takes its windows as given.
        let p = Plan::new(
            &Transform::default(),
            &ycbcr(b"yuvs", 65, 49),
            &bgra(65, 49),
        )?;
        assert_eq!(p.source_rect, pixels(0, 0, 64, 49));
        assert_eq!(p.destination_rect, rect(0, 0, 65, 49));
        // A window under a pair is kept.
        assert_eq!(even_window(3, 1, 2, 1), (3, 1));
        Ok(())
    }

    /// The conversions Apple's scaler refuses are kIOReturnUnsupported:
    /// any with 2vuy or 32ARGB either way; the rest are planned
    #[test]
    fn the_conversions_apples_scaler_refuses_are_unsupported() {
        let of = |format: ScalerFormat| SurfaceLayout {
            pixel_format: format.fourcc(),
            width: 8,
            height: 8,
            planes: Vec::new(),
            bytes_per_row: 32,
        };
        let t = Transform::default();
        let refused = |from, to| Plan::new(&t, &of(from), &of(to)).err();
        let unsupported = Some(io_return::UNSUPPORTED);
        assert_eq!(refused(ScalerFormat::Uyvy, ScalerFormat::Bgra), unsupported);
        assert_eq!(refused(ScalerFormat::Bgra, ScalerFormat::Uyvy), unsupported);
        assert_eq!(refused(ScalerFormat::Argb, ScalerFormat::Bgra), unsupported);
        assert_eq!(refused(ScalerFormat::Bgra, ScalerFormat::Argb), unsupported);
        assert_eq!(refused(ScalerFormat::Argb, ScalerFormat::Argb), unsupported);
        assert_eq!(refused(ScalerFormat::Yuyv, ScalerFormat::Bgra), None);
        assert_eq!(refused(ScalerFormat::Bgra, ScalerFormat::Yuyv), None);
        assert_eq!(refused(ScalerFormat::Bgra, ScalerFormat::Bgra), None);
        let refusals = (ScalerFormat::ALL.iter())
            .flat_map(|&a| ScalerFormat::ALL.map(|b| (a, b)))
            .filter(|&(a, b)| apple_refuses(a, b))
            .count();
        assert_eq!(refusals, 28, "64 pairs less the 36 among the other six");
    }

    #[test]
    fn ratios_past_the_capabilities_are_found_after_the_orientation() {
        // Apple's scaler's limits on the M3 Max, each case one it measured.
        let limits = [8, 8, 8, 4];
        let ratio = |sw, sh, dw, dh, bits: u64| {
            let t = Transform {
                flags: TransformFlags(bits),
                ..Transform::default()
            };
            Plan::new(&t, &bgra(sw, sh), &bgra(dw, dh))
                .unwrap_or_else(|status| panic!("refused: {status:#x}"))
                .ratio_past(limits)
        };
        assert_eq!(ratio(64, 64, 512, 64, 0), None, "8x at 8 is within");
        assert_eq!(ratio(64, 64, 1024, 64, 0), Some("MaxHUpscale"));
        assert_eq!(ratio(1024, 64, 64, 64, 0), Some("MaxHDownscale"));
        assert_eq!(ratio(64, 64, 64, 1024, 0), Some("MaxVUpscale"));
        assert_eq!(ratio(64, 512, 64, 64, 0), Some("MaxVDownscale"));
        assert_eq!(ratio(64, 256, 64, 64, 0), None, "4x down at 4");
        assert_eq!(ratio(512, 32, 32, 512, 4), None, "a quarter turn at 1x");
        assert_eq!(ratio(32, 32, 512, 32, 4), Some("MaxHUpscale"));
        assert_eq!(ratio(512, 32, 32, 512, 0), Some("MaxHDownscale"));
        // A window's fraction counts: 1.5 pixels to 13 is past 8x.
        let t = Transform {
            source_rect: FixedRect {
                x: 0,
                y: 0,
                width: FIXED_ONE + FIXED_ONE / 2,
                height: FIXED_ONE * 2,
            },
            ..Transform::default()
        };
        let plan = Plan::new(&t, &bgra(4, 4), &bgra(13, 2))
            .unwrap_or_else(|status| panic!("refused: {status:#x}"));
        assert_eq!(plan.ratio_past(limits), Some("MaxHUpscale"));
        assert_eq!(plan.ratio_past([0, 0, 0, 0]), None, "0 is no bound");
        // The largest sides and ratios do not overflow.
        let plan = Plan::new(&Transform::default(), &bgra(1, 1), &bgra(u32::MAX, 1))
            .unwrap_or_else(|status| panic!("refused: {status:#x}"));
        assert_eq!(plan.ratio_past([u32::MAX; 4]), None);
        assert_eq!(plan.ratio_past(limits), Some("MaxHUpscale"));
    }

    #[test]
    fn axes_keep_centres_where_they_enlarge_with_fix_upscaling() {
        let plan = |sw, sh, dw, dh, bits: u64| {
            let t = Transform {
                flags: TransformFlags(bits),
                ..Transform::default()
            };
            Plan::new(&t, &bgra(sw, sh), &bgra(dw, dh))
                .unwrap_or_else(|status| panic!("refused: {status:#x}"))
        };
        use Alignment::{Centres, FirstPixels};
        // A doubling: TransferSurface's (no bit 12) and TransformSurface's.
        assert_eq!(plan(32, 32, 64, 64, 0).alignment, [FirstPixels; 2]);
        assert_eq!(plan(32, 32, 64, 64, 0x1000).alignment, [Centres; 2]);
        // A halving takes the first pixels either way.
        assert_eq!(plan(64, 64, 32, 32, 0x1000).alignment, [FirstPixels; 2]);
        // Each axis by its own ratio: down across, up down the frame.
        assert_eq!(
            plan(64, 32, 32, 64, 0x1000).alignment,
            [FirstPixels, Centres]
        );
        // After the turn: 32x48 turned is 48x32, so 96x48 doubles across
        // and keeps its height.
        assert_eq!(
            plan(32, 48, 96, 32, 0x1000 | 4).alignment,
            [Centres, FirstPixels]
        );
        let p = plan(32, 48, 96, 32, 4);
        assert_eq!(p.ratios(), (0.5, 1.0));
        assert_eq!(p.shift(), (0.25, 0.0));
        assert_eq!(plan(64, 64, 32, 32, 0).shift(), (-0.5, -0.5));
        assert_eq!(Alignment::FirstPixels.source_index(3.0, 0.5), 1.5);
        assert_eq!(Alignment::Centres.source_index(3.0, 0.5), 1.25);
        // Chroma subsampled 2 to 1 keeps half the luma's offset, chroma
        // that is not the luma's, and first pixels are first pixels.
        assert_eq!(Alignment::Centres.chroma_source_index(3.0, 0.5, 2), 1.375);
        assert_eq!(Alignment::Centres.chroma_source_index(3.0, 0.5, 1), 1.25);
        assert_eq!(Alignment::FirstPixels.chroma_source_index(3.0, 0.5, 2), 1.5);
    }

    #[test]
    fn the_orientations_turn_as_apples_scaler_does() {
        // A 4x2 source: the oriented picture's top-left pixel centre.
        let at = |o: Orientation| o.source_point(0.5, 0.5, 4.0, 2.0);
        assert_eq!(at(Orientation::Identity), (0.5, 0.5));
        assert_eq!(at(Orientation::FlipVertical), (0.5, 1.5));
        assert_eq!(at(Orientation::FlipHorizontal), (3.5, 0.5));
        assert_eq!(at(Orientation::Rotate180), (3.5, 1.5));
        // Clockwise: the top-left of the result is the source's bottom-left.
        assert_eq!(at(Orientation::Rotate90), (0.5, 1.5));
        assert_eq!(at(Orientation::Rotate90FlipVertical), (3.5, 1.5));
        assert_eq!(
            at(Orientation::Rotate90FlipHorizontal),
            (0.5, 0.5),
            "the transpose"
        );
        assert_eq!(at(Orientation::Rotate270), (3.5, 0.5));
    }

    #[test]
    fn empty_windows_are_whole_surfaces() -> Result<(), IoReturn> {
        let plan = Plan::new(&Transform::default(), &bgra(64, 32), &bgra(16, 8))?;
        assert_eq!(plan.source_rect.width, 64 << 16);
        assert_eq!(
            plan.destination_rect,
            Rect {
                x: 0,
                y: 0,
                width: 16,
                height: 8
            }
        );
        assert!(plan.scales());
        assert_eq!(plan.matrix, Matrix::Bt601);
        Ok(())
    }

    #[test]
    fn options_that_change_the_output_are_refused_and_hints_are_not() {
        let with = |flags: u64| Transform {
            flags: TransformFlags(flags),
            ..Transform::default()
        };
        let plan = |flags| Plan::new(&with(flags), &bgra(16, 8), &bgra(16, 8)).err();
        assert_eq!(plan(1 << 28), Some(io_return::UNSUPPORTED), "BorderFill");
        assert_eq!(plan(1 << 40), Some(io_return::UNSUPPORTED), "HDREnable");
        assert_eq!(
            plan(2 << 36),
            Some(io_return::UNSUPPORTED),
            "ColorRemapMode"
        );
        // FixUpscaling, DitherControl 1, cositing 1, PixelAveraging.
        assert_eq!(plan(0x1000 | 1 << 13 | 1 << 15 | 1 << 32), None);
    }

    #[test]
    fn windows_outside_their_surfaces_and_unknown_formats_are_refused() {
        let t = Transform {
            destination_rect: Rect {
                x: 10,
                y: 0,
                width: 8,
                height: 8,
            },
            ..Transform::default()
        };
        assert_eq!(
            Plan::new(&t, &bgra(64, 32), &bgra(16, 8)).err(),
            Some(io_return::BAD_ARGUMENT)
        );
        let mut v210 = bgra(16, 8);
        v210.pixel_format = FourCc::new(*b"v210");
        assert_eq!(
            Plan::new(&Transform::default(), &v210, &bgra(16, 8)).err(),
            Some(io_return::UNSUPPORTED)
        );
        let mut nv12 = bgra(16, 8);
        nv12.pixel_format = FourCc::new(*b"420v");
        nv12.planes = vec![PlaneLayout {
            offset: 0,
            bytes_per_row: 16,
            width: 16,
            height: 8,
        }];
        assert_eq!(
            Plan::new(&Transform::default(), &nv12, &bgra(16, 8)).err(),
            Some(io_return::BAD_ARGUMENT),
            "one plane for a two-plane format"
        );
    }
}
