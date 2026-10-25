//! A scaler on the CPU, written from scratch: the reference a hardware
//! backend's output is compared with, and the stand-in accelerator of the
//! device's tests. Slow and plain; it is never the product's scaler.
//!
//! It works in Y'CbCr when both formats are Y'CbCr, else in RGB, each as
//! floats: luma and RGB in 0 to 1, chroma in -0.5 to 0.5. It sites chroma
//! as Apple's scaler measured to: a source is decoded with its chroma on
//! the first pixel of those it covers and interpolated between; each
//! destination pixel takes the source's pixels about it through Apple's
//! scaler's filter ([`scaler_weights`]); and a destination's chroma sample
//! is the pixel the plan's siting names, or the mean of the two. Each
//! sample point is where the plan's alignment puts the pixel, as Apple's
//! scaler measured to. [`resample`], for pictures that are not a scaler
//! format's, filters bilinearly and by area instead.

#![expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    reason = "the reference works in floats and indexes its own pictures, \
              for tests and conformance runs, never on the product's path"
)]

use crate::scaler::format::{Packing, ScalerFormat};
use crate::scaler::io_return::{self, IoReturn};
use crate::scaler::plan::{Alignment, Filter, FormatOf, Matrix, Orientation, Plan, Site, Siting};
use crate::scaler::request::FIXED_ONE;

/// A picture in the working space: four channels a pixel (Y, Cb, Cr and
/// alpha, or R, G, B and alpha).
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    /// Its width.
    pub width: usize,
    /// Its height.
    pub height: usize,
    /// Its pixels, row by row.
    pub pixels: Vec<[f64; 4]>,
}

impl Picture {
    fn at(&self, x: usize, y: usize) -> [f64; 4] {
        self.pixels[y * self.width + x]
    }
}

/// The working space for two formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// Y'CbCr, both formats being Y'CbCr.
    YCbCr,
    /// RGB.
    Rgb,
}

impl Space {
    /// The space a transform between `a` and `b` works in.
    pub fn between(a: ScalerFormat, b: ScalerFormat) -> Space {
        if a.is_ycbcr() && b.is_ycbcr() {
            Space::YCbCr
        } else {
            Space::Rgb
        }
    }
}

/// RGB to Y'CbCr.
pub fn to_ycbcr(rgb: [f64; 4], matrix: Matrix) -> [f64; 4] {
    let (kr, kb) = matrix.weights();
    let y = kr * rgb[0] + (1.0 - kr - kb) * rgb[1] + kb * rgb[2];
    [
        y,
        (rgb[2] - y) / (2.0 * (1.0 - kb)),
        (rgb[0] - y) / (2.0 * (1.0 - kr)),
        rgb[3],
    ]
}

/// Y'CbCr to RGB, each clamped to 0 to 1.
pub fn to_rgb(ycc: [f64; 4], matrix: Matrix) -> [f64; 4] {
    let (kr, kb) = matrix.weights();
    let r = ycc[0] + 2.0 * (1.0 - kr) * ycc[2];
    let b = ycc[0] + 2.0 * (1.0 - kb) * ycc[1];
    let g = (ycc[0] - kr * r - kb * b) / (1.0 - kr - kb);
    [
        r.clamp(0.0, 1.0),
        g.clamp(0.0, 1.0),
        b.clamp(0.0, 1.0),
        ycc[3],
    ]
}

/// How far a chroma code's zero lies above half the code range, as a
/// fraction of the range: Apple's scaler's chroma zero is 128.125 in 8-bit
/// codes and 512.5 in 10-bit ones, both ways. Measured on the M3 Max's
/// AppleM2ScalerCSCDriver (macOS 27.0.1, 2026-10-25): 420v and 420f into
/// BGRA matched within rounding with it, 99.3% and 99.6% of samples
/// exact, against 80% with a zero of 128; BGRA into 420v put chroma an
/// eighth of a code high, and into x420 half a code.
const CHROMA_ZERO: f64 = 1.0 / 2048.0;

/// A luma code value of `format` for `y` in 0 to 1. Apple's scaler takes a
/// code as its fraction of the largest, and video range's levels as 8-bit
/// ones, 16 and 235 of 255, at every depth: 420v into x420 wrote
/// `v * 1023 / 255` for luma `v`, not `4 v`, exactly on every sample.
/// Measured on the M3 Max's AppleM2ScalerCSCDriver (macOS 27.0.1,
/// 2026-10-25), between 420v, 420f, x420, xf20 and BGRA.
pub fn code_luma(format: ScalerFormat, y: f64) -> f64 {
    let max = f64::from((1u32 << format.bits()) - 1);
    if format.is_full_range() {
        y * max
    } else {
        (16.0 + 219.0 * y) * max / 255.0
    }
}

/// A chroma code value for `c` in -0.5 to 0.5. Apple's scaler takes a
/// chroma code as its fraction of `2^bits`, not of `2^bits - 1`, so that
/// 420v's 128 is x420's 512 (measured as [`code_luma`]), with its zero
/// [`CHROMA_ZERO`] above a half, and video range's 224 levels of 255.
pub fn code_chroma(format: ScalerFormat, c: f64) -> f64 {
    let range = f64::from(1u32 << format.bits());
    let c = if format.is_full_range() {
        c
    } else {
        c * 224.0 / 255.0
    };
    (c + 0.5 + CHROMA_ZERO) * range
}

/// The luma of a code value of `format`, in 0 to 1, as Apple's scaler
/// reads it ([`code_luma`]) for `space`. Into RGB, 10-bit video range is
/// read with its own levels, 64 and 940: x420 into BGRA fitted a red of
/// 0.29104 a code, where `(v - 64) / 876` gives 0.29110 and 8-bit levels
/// 0.29024 (measured as [`code_luma`]).
pub fn luma_of(format: ScalerFormat, v: f64, space: Space) -> f64 {
    let max = f64::from((1u32 << format.bits()) - 1);
    let scale = f64::from(1u32 << (format.bits() - 8));
    if format.is_full_range() {
        v / max
    } else if space == Space::Rgb {
        (v / scale - 16.0) / 219.0
    } else {
        (v * 255.0 / max - 16.0) / 219.0
    }
}

/// The chroma of a code value, in -0.5 to 0.5, as Apple's scaler reads it
/// ([`code_chroma`]) for `space`: into RGB, video range's levels are the
/// format's own, 896 of 1023 at 10 bits, as for [`luma_of`]; x420 into
/// BGRA fitted a blue of 0.50384 a Cb code, 1023/1024 of BT.601's.
pub fn chroma_of(format: ScalerFormat, v: f64, space: Space) -> f64 {
    let range = f64::from(1u32 << format.bits());
    let max = range - 1.0;
    let scale = f64::from(1u32 << (format.bits() - 8));
    let c = v / range - 0.5 - CHROMA_ZERO;
    if format.is_full_range() {
        c
    } else if space == Space::Rgb {
        c * max / (224.0 * scale)
    } else {
        c * 255.0 / 224.0
    }
}

/// Where a sample lives: its plane and its byte offset in the surface's
/// memory.
fn sample_offset(of: &FormatOf, plane: usize, x: usize, y: usize, component: usize) -> usize {
    let span = &of.planes[plane];
    let row = span.offset + y * span.bytes_per_row;
    match of.format.packing() {
        Packing::Biplanar420 => {
            let s = of.format.bytes_per_sample();
            row + if plane == 0 {
                x * s
            } else {
                (x * 2 + component) * s
            }
        }
        Packing::Packed422(order) => row + x * 4 + order[component],
        Packing::Rgb32(order) => row + x * 4 + order[component],
    }
}

fn read_sample(of: &FormatOf, bytes: &[u8], plane: usize, x: usize, y: usize, c: usize) -> f64 {
    let at = sample_offset(of, plane, x, y, c);
    if of.format.bytes_per_sample() == 2 {
        f64::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]) >> 6)
    } else {
        f64::from(bytes[at])
    }
}

fn write_sample(
    of: &FormatOf,
    bytes: &mut [u8],
    plane: usize,
    x: usize,
    y: usize,
    c: usize,
    v: f64,
) {
    let at = sample_offset(of, plane, x, y, c);
    let max = f64::from((1u32 << of.format.bits()) - 1);
    let v = v.round().clamp(0.0, max) as u32;
    if of.format.bytes_per_sample() == 2 {
        bytes[at..at + 2].copy_from_slice(&((v << 6) as u16).to_le_bytes());
    } else {
        bytes[at] = v as u8;
    }
}

/// Decodes a whole surface into `space`, Y'CbCr with `matrix` where it
/// converts, its chroma sited as Apple's scaler reads a source's
/// ([`Siting::APPLE`]): on the first column and row of the pixels each
/// sample covers, and interpolated between.
pub fn decode(of: &FormatOf, bytes: &[u8], space: Space, matrix: Matrix) -> Picture {
    let picture = decode_turned(of, bytes, space, Orientation::Identity, None);
    in_space(of.format, picture, space, matrix)
}

/// A picture [`decode_turned`] made of a surface of `format`, in `space`:
/// a Y'CbCr surface's read for RGB converted with `matrix`, each pixel
/// clamped as [`to_rgb`] clamps it.
fn in_space(format: ScalerFormat, picture: Picture, space: Space, matrix: Matrix) -> Picture {
    if !format.is_ycbcr() || space == Space::YCbCr {
        return picture;
    }
    Picture {
        pixels: picture
            .pixels
            .into_iter()
            .map(|p| to_rgb(p, matrix))
            .collect(),
        ..picture
    }
}

/// `picture` turned by `orientation`, pixel by pixel.
fn turn(picture: &Picture, orientation: Orientation) -> Picture {
    let (w, h) = (picture.width, picture.height);
    let (ow, oh) = if orientation.rotates() {
        (h, w)
    } else {
        (w, h)
    };
    let mut pixels = Vec::with_capacity(ow * oh);
    for y in 0..oh {
        for x in 0..ow {
            let (sx, sy) =
                orientation.source_point(x as f64 + 0.5, y as f64 + 0.5, w as f64, h as f64);
            let at = |v: f64, n: usize| (v.floor().max(0.0) as usize).min(n.saturating_sub(1));
            pixels.push(picture.at(at(sx, w), at(sy, h)));
        }
    }
    Picture {
        width: ow,
        height: oh,
        pixels,
    }
}

/// Decodes a whole surface turned by `orientation`. A Y'CbCr surface's luma is
/// turned pixel by pixel. Its chroma is that of the window `bound`, `(x, y,
/// width, height)` of the turned surface (the whole surface where none is
/// given), turned as a picture of its own, as Apple's scaler turns a source
/// window: each of the turned window's chroma samples sits on the first pixel
/// of the pixels it covers, counted from the turned window's first, and is the
/// source's sample under that pixel. So a flipped 64-wide 420v's chroma column
/// 11 comes out as column 20, not between 20 and 21, and a flipped 65-wide
/// one's window, its first 64 columns, takes chroma column 4 to 27 where the
/// surface's 33 columns would put it between 27 and 28: x420 chroma impulses
/// through 65x49 and 67x51 surfaces in each of the seven turns, and 420v
/// pictures, matched so exactly, where turning the surface's planes left them
/// 34.0 to 47.5 dB, up to 96 10-bit codes off (the M3 Max's
/// AppleM2ScalerCSCDriver, macOS 27.0.1, 2026-10-25). Chroma is interpolated
/// from the window's own samples alone, the last repeated past it, as Apple's
/// scaler reads a source window: 420v and yuvs windows into BGRA, at (13, 9)
/// and (12, 8) of 63x47 and 64x48, matched so within 1 to 3 codes, and up to 75
/// off where the window's last column took the sample past it (the same day).
///
/// A Y'CbCr surface stays Y'CbCr, its codes read for `space`
/// ([`luma_of`], [`chroma_of`]): Apple's scaler scales a Y'CbCr source's
/// chroma apart from its luma ([`Alignment::chroma_source_index`]), so a
/// conversion to RGB comes after the scale ([`in_space`]).
fn decode_turned(
    of: &FormatOf,
    bytes: &[u8],
    space: Space,
    orientation: Orientation,
    bound: Option<(f64, f64, f64, f64)>,
) -> Picture {
    let format = of.format;
    let (w, h) = (of.width as usize, of.height as usize);
    if let Packing::Rgb32(_) = format.packing() {
        let mut pixels = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let mut c = [0.0; 4];
                for (k, v) in c.iter_mut().enumerate() {
                    *v = read_sample(of, bytes, 0, x, y, k) / 255.0;
                }
                pixels.push(c);
            }
        }
        let picture = Picture {
            width: w,
            height: h,
            pixels,
        };
        return turn(&picture, orientation);
    }
    let (luma, chroma) = ycbcr_planes(of, bytes);
    let luma = turn(&luma, orientation);
    let (ssx, ssy) = format.subsampling();
    let (ssx, ssy) = (ssx as usize, ssy as usize);
    let (sx, sy) = if orientation.rotates() {
        (ssy, ssx)
    } else {
        (ssx, ssy)
    };
    let (tw, th) = (luma.width, luma.height);
    // The window in the turned surface, the whole of it where none is
    // given: its first pixel, and the last of its chroma samples.
    let (ox, oy, last_x, last_y) =
        chroma_window(bound.unwrap_or((0.0, 0.0, tw as f64, th as f64)), (sx, sy));
    // The window's chroma sample (i, j): the source's sample under the
    // turned window's pixel (ox + sx i, oy + sy j), the first of the pair
    // it covers.
    let sample = |i: isize, j: isize| {
        let x = (ox + sx * i.clamp(0, last_x as isize) as usize).min(tw - 1);
        let y = (oy + sy * j.clamp(0, last_y as isize) as usize).min(th - 1);
        let (px, py) = orientation.source_point(x as f64 + 0.5, y as f64 + 0.5, w as f64, h as f64);
        let at = |v: f64, n: usize| (v.floor().max(0.0) as usize).min(n - 1);
        chroma.at(at(px, w) / ssx, at(py, h) / ssy)
    };
    let mut pixels = Vec::with_capacity(luma.width * luma.height);
    for y in 0..luma.height {
        for x in 0..luma.width {
            // Each chroma sample on the first column and row of those it
            // covers, and between two or four the mean of their codes,
            // rounded down, as Apple's scaler takes it: 420v, 420f, x420
            // and yuvs into BGRA, and 420v into yuvs, matched so within
            // their rounding where a mean rounded to the nearest code did
            // not (the M3 Max's AppleM2ScalerCSCDriver, macOS 27.0.1,
            // 2026-10-25).
            let fx = (x as f64 - ox as f64) / sx as f64;
            let fy = (y as f64 - oy as f64) / sy as f64;
            let (x0, y0) = (fx.floor(), fy.floor());
            let (ax, ay) = (fx - x0, fy - y0);
            let mut c = [0.0; 2];
            for j in 0..2 {
                for i in 0..2 {
                    let weight =
                        if i == 1 { ax } else { 1.0 - ax } * if j == 1 { ay } else { 1.0 - ay };
                    let sample = sample(x0 as isize + i, y0 as isize + j);
                    c[0] += weight * sample[1];
                    c[1] += weight * sample[2];
                }
            }
            pixels.push([
                luma_of(format, luma.at(x, y)[0], space),
                chroma_of(format, c[0].floor(), space),
                chroma_of(format, c[1].floor(), space),
                1.0,
            ]);
        }
    }
    Picture {
        width: luma.width,
        height: luma.height,
        pixels,
    }
}

/// Where a Y'CbCr source's chroma samples lie for `window`, `(x, y, width,
/// height)` in the turned surface's pixels, each sample covering `step`
/// turned pixels, as [`decode_turned`] reads them: the window's first
/// pixel, `(x, y)` rounded down, which its first chroma sample sits on,
/// and the last of its chroma samples across and down, counted from it,
/// `(first x, first y, last across, last down)`. Its chroma is
/// interpolated from those samples alone.
pub fn chroma_window(
    window: (f64, f64, f64, f64),
    step: (usize, usize),
) -> (usize, usize, usize, usize) {
    let (x, y, w, h) = window;
    let last = |length: f64, step: usize| ((length / step as f64).ceil() as usize).max(1) - 1;
    (
        x.floor().max(0.0) as usize,
        y.floor().max(0.0) as usize,
        last(w, step.0),
        last(h, step.1),
    )
}

/// A Y'CbCr surface's luma codes, one channel, and its chroma codes, two,
/// each at its own resolution.
fn ycbcr_planes(of: &FormatOf, bytes: &[u8]) -> (Picture, Picture) {
    let format = of.format;
    let (w, h) = (of.width as usize, of.height as usize);
    let (sx, sy) = format.subsampling();
    let (cw, ch) = (w.div_ceil(sx as usize), h.div_ceil(sy as usize));
    let biplanar = matches!(format.packing(), Packing::Biplanar420);
    let mut luma = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let v = if biplanar {
                read_sample(of, bytes, 0, x, y, 0)
            } else {
                read_sample(of, bytes, 0, x / 2, y, if x & 1 == 1 { 2 } else { 0 })
            };
            luma.push([v, 0.0, 0.0, 1.0]);
        }
    }
    let mut chroma = Vec::with_capacity(cw * ch);
    for y in 0..ch {
        for x in 0..cw {
            let (cb, cr) = if biplanar {
                (
                    read_sample(of, bytes, 1, x, y, 0),
                    read_sample(of, bytes, 1, x, y, 1),
                )
            } else {
                (
                    read_sample(of, bytes, 0, x, y, 1),
                    read_sample(of, bytes, 0, x, y, 3),
                )
            };
            chroma.push([0.0, cb, cr, 1.0]);
        }
    }
    (
        Picture {
            width: w,
            height: h,
            pixels: luma,
        },
        Picture {
            width: cw,
            height: ch,
            pixels: chroma,
        },
    )
}

/// Encodes `picture`, in `space`, into a surface at `(x0, y0)`, leaving
/// the rest of the surface as it was. Each chroma sample is the pixel
/// `siting` names of those it covers, or the mean of the two it puts it
/// between; where the window leaves that out, the mean of those inside.
#[allow(clippy::too_many_arguments)]
pub fn encode(
    of: &FormatOf,
    bytes: &mut [u8],
    picture: &Picture,
    x0: usize,
    y0: usize,
    space: Space,
    matrix: Matrix,
    siting: Siting,
) {
    let format = of.format;
    let ycc = |p: [f64; 4]| match space {
        Space::Rgb => to_ycbcr(p, matrix),
        Space::YCbCr => p,
    };
    if let Packing::Rgb32(_) = format.packing() {
        for y in 0..picture.height {
            for x in 0..picture.width {
                let p = match space {
                    Space::Rgb => picture.at(x, y),
                    Space::YCbCr => to_rgb(picture.at(x, y), matrix),
                };
                for (k, v) in p.iter().enumerate() {
                    write_sample(of, bytes, 0, x0 + x, y0 + y, k, v * 255.0);
                }
            }
        }
        return;
    }
    let packed = !matches!(format.packing(), Packing::Biplanar420);
    for y in 0..picture.height {
        for x in 0..picture.width {
            let luma = code_luma(format, ycc(picture.at(x, y))[0]);
            let (px, py) = (x0 + x, y0 + y);
            if packed {
                write_sample(
                    of,
                    bytes,
                    0,
                    px / 2,
                    py,
                    if px & 1 == 1 { 2 } else { 0 },
                    luma,
                );
            } else {
                write_sample(of, bytes, 0, px, py, 0, luma);
            }
        }
    }
    let (sx, sy) = format.subsampling();
    let (sx, sy) = (sx as usize, sy as usize);
    let (x1, y1) = (x0 + picture.width, y0 + picture.height);
    // The pixels of a block of `size` along an axis its site takes.
    let takes = |site: Site, size: usize, i: usize| match (size, site) {
        (1, _) | (_, Site::Between) => true,
        (_, Site::First) => i == 0,
        (_, Site::Second) => i == 1,
    };
    for by in y0 / sy..y1.div_ceil(sy) {
        for bx in x0 / sx..x1.div_ceil(sx) {
            let mean = |named: bool| {
                let (mut cb, mut cr, mut n) = (0.0, 0.0, 0.0);
                for j in 0..sy {
                    for i in 0..sx {
                        let (px, py) = (bx * sx + i, by * sy + j);
                        if px < x0 || py < y0 || px >= x1 || py >= y1 {
                            continue;
                        }
                        if named
                            && !(takes(siting.horizontal, sx, i) && takes(siting.vertical, sy, j))
                        {
                            continue;
                        }
                        let p = ycc(picture.at(px - x0, py - y0));
                        cb += p[1];
                        cr += p[2];
                        n += 1.0;
                    }
                }
                (cb, cr, n)
            };
            let (mut cb, mut cr, mut n) = mean(true);
            if n == 0.0 {
                (cb, cr, n) = mean(false);
            }
            if n == 0.0 {
                continue;
            }
            let (cb, cr) = (code_chroma(format, cb / n), code_chroma(format, cr / n));
            if packed {
                write_sample(of, bytes, 0, bx, by, 1, cb);
                write_sample(of, bytes, 0, bx, by, 3, cr);
            } else {
                write_sample(of, bytes, 1, bx, by, 0, cb);
                write_sample(of, bytes, 1, bx, by, 1, cr);
            }
        }
    }
}

/// The weights of the source pixels along one axis for a footprint
/// centred at `centre`, `size` wide: by coverage when it is a pixel or
/// more, bilinear between the two nearest centres when less, the nearest
/// alone for [`Filter::Nearest`]. The window `[lo, hi)` bounds it: its
/// edge pixels repeat beyond it.
fn weights(centre: f64, size: f64, lo: f64, hi: f64, filter: Filter) -> Vec<(usize, f64)> {
    let first = lo.floor().max(0.0);
    let last = (hi.ceil() - 1.0).max(first);
    let index = |i: f64| i.clamp(first, last) as usize;
    if filter == Filter::Nearest {
        return vec![(index(centre.floor()), 1.0)];
    }
    if size <= 1.0 {
        let s = centre - 0.5;
        let s0 = s.floor();
        let a = s - s0;
        return vec![(index(s0), 1.0 - a), (index(s0 + 1.0), a)];
    }
    let (from, to) = ((centre - size / 2.0).max(lo), (centre + size / 2.0).min(hi));
    let mut out = Vec::new();
    let mut i = from.floor();
    while i < to {
        let w = (i + 1.0).min(to) - i.max(from);
        if w > 0.0 {
            out.push((index(i), w));
        }
        i += 1.0;
    }
    if out.is_empty() {
        out.push((index(centre.floor()), 1.0));
    }
    out
}

/// The weights of Apple's scaler's smooth filter, in 512ths of a source
/// pixel's sample, by the distance from the sample point in 32nds of a
/// source pixel: [`ENLARGING`] for an axis that enlarges, keeps its size
/// or shrinks more than 4 times, four taps; [`REDUCING`] for one that
/// shrinks by `r` up to 4 times, its row `7 - floor(8 / r)` (`floor(8 / r)`
/// from 7 down to 2), nine taps. Each phase's taps sum to 512, within the
/// measurement's rounding.
///
/// Measured on the M3 Max's AppleM2ScalerCSCDriver (macOS 27.0.1,
/// 2026-10-25), from x420 luma impulses 24 and 32 codes (8-bit) high over
/// 128, each alone in its row or column: the tables are the impulses'
/// weights at every 32nd of a pixel, across and down alike, shrinking 1.03,
/// 1.28, 1.47, 1.78, 2.34, 3.34 and 5.34 times and enlarging 1.03, 1.5
/// and 2.5 times, 33 of the 935 entries filled in between measured ones. Either side of each bound, 1.14 and 1.15, 1.33 and 1.34, 1.6
/// and 1.61, 2 and 2.02, 2.66 and 2.68, 4 and 4.02 times, the impulses took
/// the row `floor(8 / r)` names. So filtered, the suite's 420v and x420
/// scales came within a code of Apple's in luma. A sample by a step of
/// 0.45 of the range or more takes other taps ([`adaptive_blend`]).
#[rustfmt::skip]
const ENLARGING: [i16; 65] = [512, 512, 508, 500, 493, 484, 472, 457, 444, 425, 407, 388, 368, 345, 324, 300, 276, 256, 233, 211, 188, 167, 148, 128, 108, 92, 73, 60, 44, 32, 20, 9, 0, -7, -16, -20, -25, -28, -32, -32, -32, -32, -32, -32, -29, -28, -28, -24, -20, -20, -16, -16, -12, -12, -9, -7, -8, -4, -4, -3, 0, 0, 0, 0, 0];
#[rustfmt::skip]
const REDUCING: [[i16; 145]; 6] = [
    [448, 448, 444, 444, 440, 432, 424, 420, 408, 400, 388, 376, 364, 348, 336, 320, 304, 288, 272, 256, 240, 224, 208, 188, 172, 156, 140, 124, 108, 92, 80, 64, 52, 40, 28, 16, 4, -4, -12, -20, -28, -36, -40, -44, -48, -52, -56, -56, -60, -60, -60, -60, -60, -56, -56, -52, -52, -48, -44, -44, -40, -36, -32, -28, -24, -24, -20, -16, -12, -12, -8, -4, -4, 0, 0, 4, 4, 8, 8, 8, 8, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 8, 8, 8, 8, 8, 8, 8, 4, 4, 4, 4, 4, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [384, 384, 384, 380, 376, 372, 368, 364, 360, 352, 344, 336, 328, 320, 312, 300, 292, 280, 268, 256, 244, 232, 220, 208, 196, 184, 172, 160, 148, 136, 124, 112, 100, 88, 80, 68, 56, 48, 36, 28, 20, 12, 4, -4, -8, -16, -20, -24, -32, -36, -36, -40, -44, -44, -48, -48, -52, -52, -52, -52, -52, -48, -48, -48, -44, -44, -44, -40, -40, -36, -32, -32, -28, -28, -24, -20, -20, -16, -16, -12, -8, -8, -4, -4, -4, 0, 0, 4, 4, 4, 4, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 4, 4, 4, 4, 4, 4, 4, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [320, 319, 317, 316, 316, 312, 312, 308, 304, 300, 296, 292, 288, 284, 278, 272, 264, 258, 252, 244, 236, 228, 220, 212, 204, 196, 188, 180, 172, 164, 154, 144, 136, 128, 120, 112, 104, 96, 88, 80, 72, 64, 56, 48, 44, 36, 30, 24, 17, 11, 4, 0, -4, -8, -12, -16, -20, -24, -28, -28, -32, -34, -36, -36, -40, -40, -40, -40, -40, -44, -44, -44, -40, -40, -40, -40, -40, -36, -36, -35, -33, -32, -32, -28, -28, -28, -24, -24, -20, -20, -16, -16, -14, -12, -12, -10, -8, -8, -4, -4, -4, 0, 0, 0, 0, 4, 4, 4, 4, 4, 5, 7, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 3, 1, 0],
    [260, 260, 260, 260, 260, 260, 256, 256, 252, 252, 248, 248, 244, 240, 240, 236, 232, 228, 224, 220, 216, 212, 208, 204, 200, 192, 188, 184, 180, 172, 168, 164, 156, 152, 144, 140, 136, 128, 124, 116, 112, 108, 102, 96, 88, 84, 80, 74, 68, 64, 56, 52, 48, 44, 40, 36, 32, 24, 20, 16, 12, 12, 8, 4, 0, -4, -8, -8, -12, -12, -16, -20, -20, -24, -24, -24, -28, -28, -28, -32, -32, -32, -32, -32, -36, -36, -36, -36, -36, -36, -36, -32, -32, -32, -32, -32, -32, -32, -28, -28, -28, -28, -24, -24, -24, -24, -20, -20, -20, -20, -16, -16, -16, -12, -12, -12, -12, -8, -8, -8, -8, -4, -4, -4, -4, -4, 0, 0, 0, 0, 0, 0, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 8, 8, 8],
    [192, 192, 192, 192, 192, 192, 192, 192, 192, 188, 188, 188, 184, 184, 184, 180, 180, 180, 176, 176, 172, 172, 168, 168, 164, 164, 160, 160, 156, 156, 152, 148, 148, 144, 140, 140, 136, 132, 128, 128, 124, 120, 116, 116, 112, 108, 104, 100, 100, 96, 92, 88, 88, 84, 80, 76, 76, 72, 68, 64, 64, 60, 56, 52, 52, 48, 44, 44, 40, 36, 32, 32, 28, 28, 24, 22, 20, 16, 16, 12, 12, 8, 8, 4, 4, 0, 0, -4, -4, -4, -8, -8, -12, -12, -12, -16, -16, -16, -16, -20, -20, -20, -20, -20, -20, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -24, -20, -20, -20, -20, -20, -20, -20, -20, -16, -16, -16, -16, -16, -16],
    [112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 112, 108, 108, 108, 108, 108, 108, 108, 104, 104, 104, 104, 104, 100, 100, 100, 100, 100, 96, 96, 96, 96, 96, 92, 92, 92, 92, 88, 88, 88, 88, 84, 84, 84, 84, 80, 80, 80, 76, 76, 76, 76, 72, 72, 72, 68, 68, 68, 64, 64, 64, 64, 60, 60, 60, 56, 56, 56, 54, 52, 52, 52, 48, 48, 48, 44, 44, 44, 40, 40, 40, 36, 36, 36, 36, 32, 32, 32, 28, 28, 28, 28, 24, 24, 24, 24, 20, 20, 20, 16, 16, 16, 16, 16, 12, 12, 12, 12, 8, 8, 8, 8, 8, 4, 4, 4, 4, 4, 0, 0, 0, 0, 0, 0, -4, -4, -4, -4, -4, -4, -6, -8, -8, -8, -8, -8, -8],
];

/// The bank of [`REDUCING`] an axis shrinking `ratio` times takes: None
/// where it takes [`ENLARGING`]. A ratio a millionth below a bound counts as
/// at it, as the ratios a 16.16 window gives are.
fn reducing_bank(ratio: f64) -> Option<usize> {
    if ratio <= 1.0 || ratio > 4.0 {
        return None;
    }
    let banks = (8.0 / ratio + 1e-6).floor().clamp(2.0, 7.0);
    Some(7 - banks as usize)
}

/// The weights of the source pixels along one axis for a sample at
/// `centre`, in continuous coordinates (pixel `i`'s centre at `i + 0.5`),
/// of an axis that takes `ratio` source pixels a destination pixel, as
/// Apple's scaler takes them: the sample point rounded to a 32nd of a pixel
/// (halves up, measured), then [`ENLARGING`]'s four taps about it or a
/// [`REDUCING`] bank's nine ([`reducing_bank`]), over their sum. The
/// window `[lo, hi)` bounds it: its edge pixels repeat beyond it, as
/// Apple's scaler's luma measured to at 4 to 16 pixels a side, within a
/// code.
///
/// The nearest filter is filtered so too: Apple's scaler ignores
/// `UseNearestFilter`. On the M3 Max's AppleM2ScalerCSCDriver (macOS
/// 27.0.1, 2026-10-25) nine BGRA and 420v scales, 1.1 to 3 times up and 2
/// and 4 down, wrote the same bytes with it as without, and x420 impulses
/// at every 32nd of a pixel took this filter's taps with it.
fn scaler_weights(centre: f64, ratio: f64, lo: f64, hi: f64, _filter: Filter) -> Vec<(usize, f64)> {
    let first = lo.floor().max(0.0);
    let last = (hi.ceil() - 1.0).max(first);
    let index = |i: i64| (i as f64).clamp(first, last) as usize;
    let at = centre - 0.5;
    // The sample point in 32nds.
    let q = (at * 32.0 + 0.5).floor() as i64;
    let taps: Vec<(i64, i16)> = match reducing_bank(ratio) {
        None => {
            let base = q.div_euclid(32);
            (base - 1..=base + 2)
                .map(|i| (i, ENLARGING[(32 * i - q).unsigned_abs() as usize]))
                .collect()
        }
        Some(bank) => {
            let nearest = (q + 16).div_euclid(32);
            (nearest - 4..=nearest + 4)
                .map(|i| (i, REDUCING[bank][(32 * i - q).unsigned_abs() as usize]))
                .collect()
        }
    };
    let total: f64 = taps.iter().map(|&(_, w)| f64::from(w)).sum();
    taps.into_iter()
        .map(|(i, w)| (index(i), f64::from(w) / total))
        .collect()
}

/// A window of `picture`, `(x, y, width, height)` in pixels, turned by
/// `orientation` and resampled to `width` by `height`, keeping pixel
/// centres. Public for what checks pictures that are not a scaler format's,
/// as the JPEG engine's scaled decodes.
pub fn resample(
    picture: &Picture,
    window: (f64, f64, f64, f64),
    width: usize,
    height: usize,
    orientation: Orientation,
    filter: Filter,
) -> Picture {
    resample_aligned(
        picture,
        window,
        width,
        height,
        orientation,
        filter,
        [Alignment::Centres; 2],
    )
}

/// [`resample`], each destination pixel taken from where `alignment` puts
/// it across and down the destination.
pub fn resample_aligned(
    picture: &Picture,
    window: (f64, f64, f64, f64),
    width: usize,
    height: usize,
    orientation: Orientation,
    filter: Filter,
    alignment: [Alignment; 2],
) -> Picture {
    resample_with(
        picture,
        (window, width, height),
        orientation,
        filter,
        alignment,
        weights,
    )
}

/// The weights of the source pixels along one axis for a sample at a
/// centre, of an axis that takes a ratio of source pixels a destination
/// pixel, within a window: [`weights`] or [`scaler_weights`].
type Weigh = fn(f64, f64, f64, f64, Filter) -> Vec<(usize, f64)>;

/// [`resample_aligned`], each axis's taps from `weigh`, to `width` by
/// `height` of `window`.
fn resample_with(
    picture: &Picture,
    (window, width, height): ((f64, f64, f64, f64), usize, usize),
    orientation: Orientation,
    filter: Filter,
    alignment: [Alignment; 2],
    weigh: Weigh,
) -> Picture {
    let (wx, wy, ww, wh) = window;
    let rotates = orientation.rotates();
    let (ow, oh) = if rotates { (wh, ww) } else { (ww, wh) };
    let (rx, ry) = (ow / width as f64, oh / height as f64);
    let (fx, fy) = if rotates { (ry, rx) } else { (rx, ry) };
    let mut pixels = Vec::with_capacity(width * height);
    for dy in 0..height {
        for dx in 0..width {
            // In continuous coordinates of the turned window, pixel centres
            // at half units.
            let (u, v) = (
                alignment[0].source_index(dx as f64, rx) + 0.5,
                alignment[1].source_index(dy as f64, ry) + 0.5,
            );
            let (sx, sy) = orientation.source_point(u, v, ww, wh);
            let xs = weigh(wx + sx, fx, wx, (wx + ww).min(picture.width as f64), filter);
            let ys = weigh(
                wy + sy,
                fy,
                wy,
                (wy + wh).min(picture.height as f64),
                filter,
            );
            let mut acc = [0.0; 4];
            let mut total = 0.0;
            for &(y, wyt) in &ys {
                for &(x, wxt) in &xs {
                    let p = picture.at(x.min(picture.width - 1), y.min(picture.height - 1));
                    for (a, c) in acc.iter_mut().zip(p) {
                        *a += wxt * wyt * c;
                    }
                    total += wxt * wyt;
                }
            }
            pixels.push(acc.map(|a| a / total));
        }
    }
    Picture {
        width,
        height,
        pixels,
    }
}

/// Where a scale takes each pixel of a plan's destination window from, as
/// [`run`] takes it ([`taps`]), along each axis of the destination: for
/// each column of the window, then for each row, the pixels of the source
/// as turned by the plan's orientation that its filter takes about its
/// sample point, each with its weight, which may be negative. Destination
/// pixel `(x, y)` of the window is the sum, over `(i, wi)` of `across[x]`
/// and `(j, wj)` of `down[y]`, of the turned source's pixel `(i, j)` (as
/// [`run`] decodes it) weighted `wi * wj`, over the sum of those weights;
/// a Y'CbCr source's chroma takes `chroma_across` and `chroma_down` in
/// their place where they are given. A backend that applies them as they
/// are samples where the reference does, whatever rule its plan follows.
///
/// That sum is the filter's where the picture is smooth. Where a step
/// lies next to a sample point, Apple's filter takes other taps
/// ([`adaptive_blend`]): so each pass, down then across, moves each
/// destination pixel's luma, or each of its R, G and B, toward the
/// bilinear sample at its point (`across_points`, `down_points`), as
/// [`Adaptive`] says, from the six pixels about that point within the
/// axis's `span`.
#[derive(Clone, Debug, PartialEq)]
pub struct Taps {
    /// The turned source's width and height.
    pub turned: (usize, usize),
    /// The source window, `(x, y, width, height)` in the turned source's
    /// pixels, whose chroma [`run`] interpolates from ([`decode_turned`]).
    pub window: (f64, f64, f64, f64),
    /// Each column's pixels of the turned source, by column, and weights.
    pub across: Vec<Vec<(usize, f64)>>,
    /// Each row's pixels of the turned source, by row, and weights.
    pub down: Vec<Vec<(usize, f64)>>,
    /// A Y'CbCr source's chroma's, by column, along an axis where its
    /// chroma samples elsewhere than its luma: one that keeps pixel centres
    /// and that the source subsamples ([`Alignment::chroma_source_index`]).
    /// None where it takes `across`.
    pub chroma_across: Option<Vec<Vec<(usize, f64)>>>,
    /// The same by row, None where it takes `down`.
    pub chroma_down: Option<Vec<Vec<(usize, f64)>>>,
    /// Each column's sample point, rounded to a 32nd of a pixel as the
    /// filter rounds it: the turned source's pixel it lies in, which may
    /// lie before the first, and its fraction of a pixel past that pixel.
    pub across_points: Vec<(i64, f64)>,
    /// Each row's sample point, the same.
    pub down_points: Vec<(i64, f64)>,
    /// The turned source's pixels the taps of each column are clamped to,
    /// first and last: the window's edge pixels repeat beyond it.
    pub across_span: (usize, usize),
    /// The same for each row.
    pub down_span: (usize, usize),
    /// Whether each axis, across and down, reduces: more than one source
    /// pixel a destination pixel.
    pub reduces: [bool; 2],
}

/// The pixels each destination pixel of `plan` takes, and their weights
/// ([`Taps`]): the turned source window's, about the sample point the
/// plan's alignment puts its pixel at, by [`scaler_weights`].
pub fn taps(plan: &Plan) -> Taps {
    let src = &plan.source_format;
    let fixed = |v: u64| v as f64 / FIXED_ONE as f64;
    let window = turned_window(
        (
            fixed(plan.source_rect.x),
            fixed(plan.source_rect.y),
            fixed(plan.source_rect.width),
            fixed(plan.source_rect.height),
        ),
        plan.orientation,
        f64::from(src.width),
        f64::from(src.height),
    );
    let (w, h) = (src.width as usize, src.height as usize);
    let turned = if plan.orientation.rotates() {
        (h, w)
    } else {
        (w, h)
    };
    let (wx, wy, ww, wh) = window;
    // A Y'CbCr source's chroma subsampling, along the turned axes.
    let subsampling = src.format.is_ycbcr().then(|| {
        let (sx, sy) = src.format.subsampling();
        if plan.orientation.rotates() {
            [sy, sx]
        } else {
            [sx, sy]
        }
    });
    // Where destination pixel `d` samples along axis `k`, of `ratio`:
    // the luma's place, or a Y'CbCr source's chroma's.
    let place = |k: usize, chroma: bool, d: f64, ratio: f64| match subsampling {
        Some(s) if chroma => plan.alignment[k].chroma_source_index(d, ratio, s[k]),
        _ => plan.alignment[k].source_index(d, ratio),
    };
    // Whether a Y'CbCr source's chroma samples apart from its luma along
    // axis `k`.
    let apart =
        |k: usize| subsampling.is_some_and(|s| s[k] > 1 && plan.alignment[k] == Alignment::Centres);
    let axis = |origin: f64, length: f64, limit: usize, count: u32, k: usize, chroma: bool| {
        let ratio = length / f64::from(count);
        (0..count)
            .map(|d| {
                let at = place(k, chroma, f64::from(d), ratio) + 0.5;
                scaler_weights(
                    origin + at,
                    ratio,
                    origin,
                    (origin + length).min(limit as f64),
                    plan.filter,
                )
                .into_iter()
                .map(|(i, weight)| (i.min(limit - 1), weight))
                .collect()
            })
            .collect()
    };
    // Each pixel's sample point, as `scaler_weights` rounds it.
    let points = |origin: f64, length: f64, count: u32, k: usize| {
        let ratio = length / f64::from(count);
        (0..count)
            .map(|d| {
                let at = origin + place(k, false, f64::from(d), ratio);
                let q = (at * 32.0 + 0.5).floor() as i64;
                let pixel = q.div_euclid(32);
                (pixel, (q - 32 * pixel) as f64 / 32.0)
            })
            .collect()
    };
    // The pixels `scaler_weights` clamps an axis's taps to.
    let span = |origin: f64, length: f64, limit: usize| {
        let first = origin.floor().max(0.0);
        let last = ((origin + length).min(limit as f64).ceil() - 1.0).max(first);
        (
            (first as usize).min(limit - 1),
            (last as usize).min(limit - 1),
        )
    };
    let (dw, dh) = (plan.destination_rect.width, plan.destination_rect.height);
    Taps {
        turned,
        window,
        across: axis(wx, ww, turned.0, dw, 0, false),
        down: axis(wy, wh, turned.1, dh, 1, false),
        chroma_across: apart(0).then(|| axis(wx, ww, turned.0, dw, 0, true)),
        chroma_down: apart(1).then(|| axis(wy, wh, turned.1, dh, 1, true)),
        across_points: points(wx, ww, dw, 0),
        down_points: points(wy, wh, dh, 1),
        across_span: span(wx, ww, turned.0),
        down_span: span(wy, wh, turned.1),
        reduces: [ww > f64::from(dw), wh > f64::from(dh)],
    }
}

/// The step between neighbouring pixels, of the full range, from which
/// Apple's filter takes its other taps ([`adaptive_blend`]).
pub const ADAPTIVE_STEP: f64 = 0.45;

/// The smallest step, of the full range, that keeps a reduction from
/// taking its other taps ([`adaptive_blend`]): measured between 12 codes of
/// 255 and 16, and taken at 14.5, which no step of 8 or 10 bits lies on.
pub const ADAPTIVE_SMALL: f64 = 14.5 / 255.0;

/// How far Apple's filter moves a sample from its taps' sum `a` toward the
/// bilinear sample at its point, as a fraction of the way, by the five
/// steps between the six pixels about the point, `steps[k]` the one
/// between pixels `b + k - 2` and `b + k - 1`, `b` the pixel the point lies
/// in, each as a fraction of the full range ([`Adaptive`]). A step of
/// [`ADAPTIVE_STEP`] or more next to the point (`steps[1]` or `steps[3]`)
/// moves an enlargement's sample 0.6 of the way. A reduction's moves 0.6
/// where it is the only such step of the five, 0.45 where it is one of
/// two, and not at all where there are more, or where any other step is of
/// [`ADAPTIVE_SMALL`] or more. Below it an enlargement's moves
/// `(255 m - 32) / 500` of the way, `m` the largest step, up to a quarter,
/// and a reduction's stays.
///
/// Measured on the M3 Max's AppleM2ScalerCSCDriver (macOS 27.0.1,
/// 2026-10-25) with the gap probe, on x420 and BGRA pictures of our own
/// ([bugs.md](../../../../docs/video/bugs.md), B83): x420 luma impulses
/// moved from 461 10-bit codes high, not at 460, and BGRA red impulses
/// over backgrounds of 0 to 160 from 0.4508 in light, not at 0.4498; every
/// set of six neighbouring pixels raised by 112, 120 and 160 codes,
/// doubled and halved, moved as this says at every sample; impulses
/// moved alike 1.25 to 8 times up, steps and pairs as their steps say. The
/// sample so moved matched 0.6 of the way to the bilinear sample within
/// 0.002 of the impulse a tap. A reduction's isolated feature below the
/// step, which moves it up to 0.2 of the way unless another lies within
/// three pixels, is not modelled.
pub fn adaptive_blend(steps: [f64; 5], reduces: bool) -> f64 {
    let outer = steps[1].max(steps[3]);
    if !reduces {
        if outer >= ADAPTIVE_STEP {
            return 0.6;
        }
        let largest = steps.iter().copied().fold(0.0, f64::max);
        return ((255.0 * largest - 32.0) / 500.0).clamp(0.0, 0.25);
    }
    if outer < ADAPTIVE_STEP {
        return 0.0;
    }
    let big = steps.iter().filter(|&&d| d >= ADAPTIVE_STEP).count();
    let small = (steps.iter())
        .filter(|&&d| (ADAPTIVE_SMALL..ADAPTIVE_STEP).contains(&d))
        .count();
    match (big, small) {
        (1, 0) => 0.6,
        (2, 0) => 0.45,
        _ => 0.0,
    }
}

/// Which channels of a transform's working picture Apple's filter moves
/// as [`adaptive_blend`] says, and how a step between two of its values is
/// a fraction of the full range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Adaptive {
    /// The channels from the first: 1, a Y'CbCr source's luma; or 3, an
    /// RGB source's R, G and B, each its own; never chroma or alpha.
    pub channels: usize,
    /// A step's fraction of the full range a unit of the working value:
    /// of a Y'CbCr source's codes over their largest, as [`luma_of`] reads
    /// them; an RGB source's light as it is.
    pub scale: f64,
}

/// [`Adaptive`] for a transform from `from` in `space`.
pub fn adaptive(from: ScalerFormat, space: Space) -> Adaptive {
    if !from.is_ycbcr() {
        return Adaptive {
            channels: 3,
            scale: 1.0,
        };
    }
    let max = f64::from((1u32 << from.bits()) - 1);
    let per_code = luma_of(from, 1.0, space) - luma_of(from, 0.0, space);
    Adaptive {
        channels: 1,
        scale: 1.0 / (per_code * max),
    }
}

/// `value`, a pass's taps' sum at a point lying in pixel `b` a fraction
/// `f` past it, moved as [`adaptive_blend`] says by the pixels `at` gives
/// about it, clamped to `span`.
fn adapt(
    value: &mut [f64; 4],
    at: impl Fn(usize) -> [f64; 4],
    (b, f): (i64, f64),
    (first, last): (usize, usize),
    reduces: bool,
    adaptive: Adaptive,
) {
    let pixel = |k: i64| at((b - 2 + k).clamp(first as i64, last as i64) as usize);
    let six: [[f64; 4]; 6] = std::array::from_fn(|k| pixel(k as i64));
    for c in 0..adaptive.channels {
        let steps: [f64; 5] =
            std::array::from_fn(|k| (six[k + 1][c] - six[k][c]).abs() * adaptive.scale);
        let beta = adaptive_blend(steps, reduces);
        let bilinear = (1.0 - f) * six[2][c] + f * six[3][c];
        value[c] += beta * (bilinear - value[c]);
    }
}

/// `picture`, the turned source, resampled as `taps` says: down each column
/// the across pass reads, then across, each pass's sums moved as
/// `adaptive` says ([`adapt`]), the second over the first's output as it
/// is, unclamped, as Apple's scaler's two passes measured to
/// ([`adaptive_blend`]).
fn resample_taps(picture: &Picture, taps: &Taps, adaptive: Adaptive) -> Picture {
    let (width, height) = (taps.across.len(), taps.down.len());
    // A pass's sum of `values` by `ws`, over the sum of the weights.
    let weigh = |ws: &[(usize, f64)], values: &dyn Fn(usize) -> [f64; 4]| {
        let mut acc = [0.0; 4];
        let mut total = 0.0;
        for &(i, w) in ws {
            let v = values(i);
            for (a, c) in acc.iter_mut().zip(v) {
                *a += w * c;
            }
            total += w;
        }
        acc.map(|a| a / total)
    };
    // One pass's pixel: the luma's taps, the chroma's where it has its own,
    // and the luma or R, G and B moved.
    let pass = |ws: &[(usize, f64)],
                chroma: Option<&[(usize, f64)]>,
                values: &dyn Fn(usize) -> [f64; 4],
                point: (i64, f64),
                span: (usize, usize),
                reduces: bool| {
        let mut v = weigh(ws, values);
        if let Some(cs) = chroma {
            let c = weigh(cs, values);
            (v[1], v[2]) = (c[1], c[2]);
        }
        adapt(&mut v, values, point, span, reduces, adaptive);
        v
    };
    let (x0, x1) = taps.across_span;
    let columns = x1 - x0 + 1;
    let mut lines = Vec::with_capacity(columns * height);
    for (dy, ys) in taps.down.iter().enumerate() {
        let chroma = taps.chroma_down.as_ref().map(|c| c[dy].as_slice());
        for x in x0..=x1 {
            let column = |y: usize| picture.at(x, y);
            lines.push(pass(
                ys,
                chroma,
                &column,
                taps.down_points[dy],
                taps.down_span,
                taps.reduces[1],
            ));
        }
    }
    let mut pixels = Vec::with_capacity(width * height);
    for dy in 0..height {
        let line = &lines[dy * columns..(dy + 1) * columns];
        let row = |x: usize| line[x - x0];
        for (dx, xs) in taps.across.iter().enumerate() {
            let chroma = taps.chroma_across.as_ref().map(|c| c[dx].as_slice());
            pixels.push(pass(
                xs,
                chroma,
                &row,
                taps.across_points[dx],
                taps.across_span,
                taps.reduces[0],
            ));
        }
    }
    Picture {
        width,
        height,
        pixels,
    }
}

/// Where `window`, `(x, y, width, height)` of a `width` by `height`
/// source, is in the source turned by `orientation`.
fn turned_window(
    window: (f64, f64, f64, f64),
    orientation: Orientation,
    width: f64,
    height: f64,
) -> (f64, f64, f64, f64) {
    // The turn's map from the turned picture to the source is affine, a
    // signed permutation and an offset; its inverse is the transpose.
    let at = |x, y| orientation.source_point(x, y, width, height);
    let origin = at(0.0, 0.0);
    let (across, down) = (at(1.0, 0.0), at(0.0, 1.0));
    let (a, b) = (across.0 - origin.0, across.1 - origin.1);
    let (c, d) = (down.0 - origin.0, down.1 - origin.1);
    let back = |(x, y): (f64, f64)| {
        let (dx, dy) = (x - origin.0, y - origin.1);
        (a * dx + b * dy, c * dx + d * dy)
    };
    let (wx, wy, ww, wh) = window;
    let (p, q) = (back((wx, wy)), back((wx + ww, wy + wh)));
    (
        p.0.min(q.0),
        p.1.min(q.1),
        (p.0 - q.0).abs(),
        (p.1 - q.1).abs(),
    )
}

/// sRGB's decoding of a code, 0 to 1, to linear light, 0 to 1. Apple's
/// scaler scales an RGB source in light so decoded, into RGB or Y'CbCr:
/// the M3 Max's AppleM2ScalerCSCDriver (macOS 27.0.1, 2026-10-25) scaling
/// the conformance suite's BGRA pictures 2 to 8 times up and 1.5 to 4 down
/// matched the reference so within a code on red and green at 97% to 99%
/// of samples and 1 to 3 codes on the rest, where the reference scaling
/// codes was up to 61 off. BGRA impulses of 32 codes scaled 2 times up,
/// 1.03 to 2 down, across and down, into x420 and 420v matched it within
/// a code, where scaling codes left x420's luma 4 to 6 codes off, and the
/// test picture's samples whose taps spanned under 24 codes matched it
/// within a code (the rest differ at sharp edges, [`adaptive_blend`]). Its
/// own curve is not known to be sRGB's, only to fit it so.
fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// The inverse of [`srgb_to_linear`], of light clamped to 0 to 1: a scale
/// that rings past black or white is clamped before it is converted to
/// Y'CbCr. RGB test pictures scaled into x420 (as [`srgb_to_linear`]) were
/// up to 32 codes off Apple's luma without the clamp and 14 with it, the
/// rest at their sharpest edges.
fn linear_to_srgb(l: f64) -> f64 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// Whether a transform from `from` into `to` resamples in linear light
/// ([`srgb_to_linear`]): from an RGB format, into RGB or Y'CbCr.
pub fn in_linear_light(from: ScalerFormat, _to: ScalerFormat) -> bool {
    !from.is_ycbcr()
}

/// `picture` with `f` applied to each colour component, alpha kept.
fn light(picture: &Picture, f: fn(f64) -> f64) -> Picture {
    Picture {
        width: picture.width,
        height: picture.height,
        pixels: picture
            .pixels
            .iter()
            .map(|p| [f(p[0]), f(p[1]), f(p[2]), p[3]])
            .collect(),
    }
}

/// Runs `plan` from `source`'s bytes into `destination`'s, as a scaler
/// would: only the destination window changes. Between two Y'CbCr formats
/// the picture is resampled in Y'CbCr, so a copy or a turn into a
/// destination sited as a source is read is exact; otherwise it goes
/// through RGB with the plan's matrix.
pub fn run(plan: &Plan, source: &[u8], destination: &mut [u8]) -> Result<(), IoReturn> {
    if !plan.source_format.fits(source.len()) || !plan.destination_format.fits(destination.len()) {
        return Err(io_return::BAD_ARGUMENT);
    }
    let (src, dst) = (&plan.source_format, &plan.destination_format);
    let rect = plan.destination_rect;
    let (rx, ry) = (rect.x as usize, rect.y as usize);
    let space = Space::between(src.format, dst.format);
    // The source turned first, as Apple's scaler turns it
    // ([`decode_turned`]), its chroma read within the turned window, then
    // the window resampled as [`taps`] says.
    let taps = taps(plan);
    let picture = decode_turned(src, source, space, plan.orientation, Some(taps.window));
    // An RGB source scales in linear light, into RGB or Y'CbCr, as Apple's
    // scaler's does ([`srgb_to_linear`]).
    let linear = in_linear_light(src.format, dst.format);
    let picture = if linear {
        light(&picture, srgb_to_linear)
    } else {
        picture
    };
    let out = resample_taps(&picture, &taps, adaptive(src.format, space));
    let out = if linear {
        light(&out, linear_to_srgb)
    } else {
        out
    };
    // A Y'CbCr source into RGB converts after the scale.
    let out = in_space(src.format, out, space, plan.matrix);
    encode(
        dst,
        destination,
        &out,
        rx,
        ry,
        space,
        plan.matrix,
        plan.destination_siting,
    );
    Ok(())
}

/// The PSNR of `a` against `b`, two surfaces of `of`, per plane, over each
/// row's samples (10-bit samples against 1023, others against 255);
/// infinite where they are equal.
pub fn psnr(of: &FormatOf, a: &[u8], b: &[u8]) -> Vec<f64> {
    of.planes
        .iter()
        .map(|span| {
            let (mut se, mut n) = (0.0f64, 0usize);
            let peak: f64 = if of.format.bytes_per_sample() == 2 {
                1023.0
            } else {
                255.0
            };
            for row in 0..span.rows {
                let start = span.offset + row * span.bytes_per_row;
                let (ra, rb) = (
                    &a[start..start + span.row_bytes],
                    &b[start..start + span.row_bytes],
                );
                if of.format.bytes_per_sample() == 2 {
                    for (x, y) in ra.as_chunks::<2>().0.iter().zip(rb.as_chunks::<2>().0) {
                        let d = f64::from(u16::from_le_bytes([x[0], x[1]]) >> 6)
                            - f64::from(u16::from_le_bytes([y[0], y[1]]) >> 6);
                        se += d * d;
                        n += 1;
                    }
                } else {
                    for (x, y) in ra.iter().zip(rb) {
                        let d = f64::from(*x) - f64::from(*y);
                        se += d * d;
                        n += 1;
                    }
                }
            }
            if se == 0.0 || n == 0 {
                f64::INFINITY
            } else {
                10.0 * (peak * peak / (se / n as f64)).log10()
            }
        })
        .collect()
}

/// The test picture every check draws, from scratch: red ramps left to
/// right, green top to bottom, blue is a disc off centre, and a
/// checkerboard of 16-pixel squares darkens every other square, so that a
/// flip, a rotation, a crop and a scale each show. RGB, 0 to 1.
pub fn test_picture(width: usize, height: usize) -> Picture {
    let mut pixels = Vec::with_capacity(width * height);
    let short = width.min(height) as f64;
    for y in 0..height {
        for x in 0..width {
            let dx = (x as f64 - 0.3 * width as f64) / (0.25 * short);
            let dy = (y as f64 - 0.35 * height as f64) / (0.25 * short);
            let check = if (x / 16 + y / 16) & 1 == 1 {
                0.85
            } else {
                1.0
            };
            pixels.push([
                (0.1 + 0.8 * x as f64 / (width.max(2) - 1) as f64) * check,
                (0.1 + 0.8 * y as f64 / (height.max(2) - 1) as f64) * check,
                if dx * dx + dy * dy < 1.0 { 0.85 } else { 0.2 } * check,
                1.0,
            ]);
        }
    }
    Picture {
        width,
        height,
        pixels,
    }
}

/// A uniform RGB picture.
pub fn flat_picture(width: usize, height: usize, rgb: [f64; 4]) -> Picture {
    Picture {
        width,
        height,
        pixels: vec![rgb; width * height],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fourcc::FourCc;
    use crate::scaler::request::{FixedRect, Rect, Transform, TransformFlags};
    use crate::surfaces::{PlaneLayout, SurfaceLayout};

    pub(crate) fn layout(format: ScalerFormat, width: u32, height: u32) -> (SurfaceLayout, usize) {
        let row = |bytes: usize| bytes.div_ceil(64) * 64;
        match format.packing() {
            Packing::Biplanar420 => {
                let s = format.bytes_per_sample();
                let luma = row(width as usize * s);
                let chroma = row(width.div_ceil(2) as usize * 2 * s);
                let luma_size = luma * height as usize;
                (
                    SurfaceLayout {
                        pixel_format: format.fourcc(),
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
                                offset: luma_size,
                                bytes_per_row: chroma,
                                width: width.div_ceil(2),
                                height: height.div_ceil(2),
                            },
                        ],
                        bytes_per_row: 0,
                    },
                    luma_size + chroma * height.div_ceil(2) as usize,
                )
            }
            Packing::Packed422(_) | Packing::Rgb32(_) => {
                let bytes = if format.is_ycbcr() {
                    width.div_ceil(2) as usize * 4
                } else {
                    width as usize * 4
                };
                let bpr = row(bytes);
                (
                    SurfaceLayout {
                        pixel_format: format.fourcc(),
                        width,
                        height,
                        planes: Vec::new(),
                        bytes_per_row: bpr,
                    },
                    bpr * height as usize,
                )
            }
        }
    }

    fn surface(
        format: ScalerFormat,
        width: u32,
        height: u32,
        picture: &Picture,
    ) -> (FormatOf, Vec<u8>) {
        let (l, size) = layout(format, width, height);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let mut bytes = vec![0u8; size];
        encode(
            &of,
            &mut bytes,
            picture,
            0,
            0,
            Space::Rgb,
            Matrix::Bt601,
            Siting::CENTRED,
        );
        (of, bytes)
    }

    fn plan(
        src: ScalerFormat,
        sw: u32,
        sh: u32,
        dst: ScalerFormat,
        dw: u32,
        dh: u32,
        t: Transform,
    ) -> Plan {
        let (s, _) = layout(src, sw, sh);
        let (d, _) = layout(dst, dw, dh);
        Plan::of_any_pair(&t, &s, &d).unwrap_or_else(|status| panic!("refused: {status:#x}"))
    }

    #[test]
    fn a_copy_is_exact_and_a_round_trip_through_rgb_is_close() {
        let picture = test_picture(32, 16);
        for format in ScalerFormat::ALL {
            let (of, src) = surface(format, 32, 16, &picture);
            let p = plan(format, 32, 16, format, 32, 16, Transform::default());
            let mut dst = vec![0u8; src.len()];
            assert_eq!(run(&p, &src, &mut dst), Ok(()));
            assert!(
                psnr(&of, &dst, &src).iter().all(|v| *v > 50.0),
                "{format:?}: {:?}",
                psnr(&of, &dst, &src)
            );
        }
    }

    /// Every turn between two surfaces of one Y'CbCr format moves each
    /// plane's samples as they are, as Apple's scaler turns a 420v: no
    /// chroma is interpolated.
    #[test]
    fn a_turn_between_ycbcr_surfaces_moves_the_planes_samples() {
        let picture = test_picture(32, 16);
        for format in [ScalerFormat::Nv12VideoRange, ScalerFormat::P010VideoRange] {
            let (of, src) = surface(format, 32, 16, &picture);
            for bits in 1..8 {
                let orientation = Orientation::from_bits(bits);
                let (w, h) = if orientation.rotates() {
                    (16, 32)
                } else {
                    (32, 16)
                };
                let t = Transform {
                    flags: TransformFlags(u64::from(bits)),
                    ..Transform::default()
                };
                let p = plan(format, 32, 16, format, w, h, t);
                let (_, size) = layout(format, w, h);
                let mut dst = vec![0u8; size];
                assert_eq!(run(&p, &src, &mut dst), Ok(()));
                let (_, chroma) = ycbcr_planes(&of, &src);
                let (_, out) = ycbcr_planes(&p.destination_format, &dst);
                assert_eq!(
                    turn(&chroma, orientation),
                    out,
                    "{format:?}, transform {bits}"
                );
            }
        }
    }

    /// A destination's chroma sample is the pixel the transform's siting
    /// names, or the mean of the two: a picture whose every pixel has
    /// chroma of its own, into 420v with each siting.
    #[test]
    fn each_siting_writes_the_pixels_it_names() {
        let mut picture = flat_picture(4, 4, [0.0; 4]);
        for y in 0..4 {
            for x in 0..4 {
                picture.pixels[y * 4 + x] = [0.5, x as f64 / 10.0, y as f64 / 10.0, 1.0];
            }
        }
        let (l, size) = layout(ScalerFormat::Nv12FullRange, 4, 4);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        // Cb by column, Cr by row, of chroma sample (0, 0), for bits 0 to 6.
        let want = [
            (0.0, 0.0),
            (0.0, 0.5),
            (0.5, 0.5),
            (0.0, 0.0),
            (0.5, 0.0),
            (0.0, 1.0),
            (0.5, 1.0),
        ];
        for (bits, (cb, cr)) in want.into_iter().enumerate() {
            let mut bytes = vec![0u8; size];
            let siting = Siting::of_cosite(bits as u32);
            encode(
                &of,
                &mut bytes,
                &picture,
                0,
                0,
                Space::YCbCr,
                Matrix::Bt601,
                siting,
            );
            let chroma = &bytes[of.planes[1].offset..];
            let code = |v: f64| (128.0 + v / 10.0 * 255.0).round() as u8;
            assert_eq!((chroma[0], chroma[1]), (code(cb), code(cr)), "bits {bits}");
        }
    }

    #[test]
    fn a_rotation_by_four_quarter_turns_comes_back() {
        let picture = test_picture(24, 16);
        let (of, src) = surface(ScalerFormat::Bgra, 24, 16, &picture);
        let t = Transform {
            flags: TransformFlags(4),
            ..Transform::default()
        };
        let p = plan(
            ScalerFormat::Bgra,
            24,
            16,
            ScalerFormat::Bgra,
            16,
            24,
            t.clone(),
        );
        let (l, size) = layout(ScalerFormat::Bgra, 16, 24);
        let mut once = vec![0u8; size];
        assert_eq!(run(&p, &src, &mut once), Ok(()));
        let back = plan(
            ScalerFormat::Bgra,
            16,
            24,
            ScalerFormat::Bgra,
            24,
            16,
            t.clone(),
        );
        let mut twice = vec![0u8; src.len()];
        assert_eq!(run(&back, &once, &mut twice), Ok(()));
        // Two quarter turns are a half turn: the top-left pixel is the
        // source's bottom-right.
        let of_once = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let row = of.planes[0].bytes_per_row;
        assert_eq!(&twice[..4], &src[15 * row + 23 * 4..15 * row + 24 * 4]);
        // After one turn, the top-left pixel is the source's bottom-left.
        assert_eq!(&once[..4], &src[15 * row..15 * row + 4]);
        assert_eq!(of_once.width, 16);
    }

    #[test]
    fn the_destination_window_alone_changes() {
        let (_, src) = surface(ScalerFormat::Nv12VideoRange, 16, 16, &test_picture(16, 16));
        let grey = flat_picture(64, 32, [0.5, 0.5, 0.5, 1.0]);
        let (of, before) = surface(ScalerFormat::Nv12VideoRange, 64, 32, &grey);
        let t = Transform {
            destination_rect: Rect {
                x: 8,
                y: 8,
                width: 16,
                height: 16,
            },
            ..Transform::default()
        };
        let p = plan(
            ScalerFormat::Nv12VideoRange,
            16,
            16,
            ScalerFormat::Nv12VideoRange,
            64,
            32,
            t,
        );
        let mut after = before.clone();
        assert_eq!(run(&p, &src, &mut after), Ok(()));
        let row = of.planes[0].bytes_per_row;
        assert_eq!(after[7 * row..7 * row + 64], before[7 * row..7 * row + 64]);
        assert_ne!(
            after[8 * row + 8..8 * row + 24],
            before[8 * row + 8..8 * row + 24]
        );
        assert_eq!(
            after[8 * row + 24..8 * row + 64],
            before[8 * row + 24..8 * row + 64]
        );
    }

    #[test]
    fn an_odd_surface_turns_its_window_as_a_picture() {
        // A 65x5 420v whose Cb sample (4, 1) is raised: its window is the
        // even 64x4 at (0, 0), and turned, each of the window's chroma
        // samples lands whole on one of the destination's.
        let n = ScalerFormat::Nv12VideoRange;
        let (l, size) = layout(n, 65, 5);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let mut src = vec![128u8; size];
        write_sample(&of, &mut src, 1, 4, 1, 0, 192.0);
        let cb = |bits: u64, (dw, dh): (u32, u32)| {
            let t = Transform {
                flags: TransformFlags(bits),
                ..Transform::default()
            };
            let p = plan(n, 65, 5, n, dw, dh, t);
            let (dl, size) = layout(n, dw, dh);
            let dof = FormatOf::of(&dl).unwrap_or_else(|status| panic!("refused: {status:#x}"));
            let mut dst = vec![128u8; size];
            assert_eq!(run(&p, &src, &mut dst), Ok(()));
            let (cw, ch) = (dw.div_ceil(2) as usize, dh.div_ceil(2) as usize);
            let raised: Vec<(usize, usize, f64)> = (0..ch)
                .flat_map(|y| (0..cw).map(move |x| (x, y)))
                .map(|(x, y)| (x, y, read_sample(&dof, &dst, 1, x, y, 0)))
                .filter(|&(_, _, v)| v != 128.0)
                .collect();
            raised
        };
        // Left to right: the window's 32 columns reversed, 4 to 27; the
        // surface's 33 would put it between 27 and 28.
        assert_eq!(cb(2, (65, 5)), vec![(27, 1, 192.0)]);
        // Turned 180 degrees: its rows too, 1 to 0.
        assert_eq!(cb(3, (65, 5)), vec![(27, 0, 192.0)]);
        // Turned 90 degrees clockwise into 5x65: the window's chroma row 1
        // becomes column 0, its column 4 row 4.
        assert_eq!(cb(4, (5, 65)), vec![(0, 4, 192.0)]);
    }

    /// Where a scale takes its pixels from, as the plan's alignment says: a
    /// BGRA ramp, red 16 + 6x, doubled with and without `FixUpscaling` and
    /// halved, read back away from the edges.
    #[test]
    fn an_odd_ycbcr_copy_leaves_its_last_column_and_row() {
        let n = ScalerFormat::Nv12VideoRange;
        let (l, size) = layout(n, 5, 5);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let (luma, chroma) = (of.planes[0], of.planes[1]);
        let mut src = vec![0u8; size];
        for y in 0..5 {
            src[luma.offset + y * luma.bytes_per_row..][..5].fill(100);
        }
        for y in 0..3 {
            src[chroma.offset + y * chroma.bytes_per_row..][..6].fill(90);
        }
        let mut dst = vec![7u8; size];
        let p = plan(n, 5, 5, n, 5, 5, Transform::default());
        assert_eq!(run(&p, &src, &mut dst), Ok(()));
        for y in 0..5 {
            let row = &dst[luma.offset + y * luma.bytes_per_row..][..5];
            let want: &[u8] = if y < 4 {
                &[100, 100, 100, 100, 7]
            } else {
                &[7; 5]
            };
            assert_eq!(row, want, "luma row {y}");
        }
        for y in 0..3 {
            let row = &dst[chroma.offset + y * chroma.bytes_per_row..][..6];
            let want: &[u8] = if y < 2 {
                &[90, 90, 90, 90, 7, 7]
            } else {
                &[7; 6]
            };
            assert_eq!(row, want, "chroma row {y}");
        }
    }

    #[test]
    fn a_source_window_bounds_its_chroma() {
        // An 8x2 420v whose chroma changes past its first four columns:
        // its 4x2 window's last column takes chroma column 1 alone.
        let n = ScalerFormat::Nv12VideoRange;
        let (l, size) = layout(n, 8, 2);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let (luma, chroma) = (of.planes[0], of.planes[1]);
        let mut src = vec![0u8; size];
        for y in 0..2 {
            src[luma.offset + y * luma.bytes_per_row..][..8].fill(128);
        }
        src[chroma.offset..][..8].copy_from_slice(&[128, 128, 128, 128, 240, 16, 240, 16]);
        let bgra = |t: Transform| {
            let p = plan(n, 8, 2, ScalerFormat::Bgra, 4, 2, t);
            let (_, size) = layout(ScalerFormat::Bgra, 4, 2);
            let mut dst = vec![0u8; size];
            assert_eq!(run(&p, &src, &mut dst), Ok(()));
            dst[..16].to_vec()
        };
        let window = bgra(Transform {
            source_rect: FixedRect {
                x: 0,
                y: 0,
                width: 4 * FIXED_ONE,
                height: 2 * FIXED_ONE,
            },
            ..Transform::default()
        });
        assert_eq!(window[8..12], window[12..16], "{window:?}");
        assert_eq!(window[8..12], window[0..4], "{window:?}");
    }

    #[test]
    fn the_filter_takes_apples_banks() {
        // floor(8 / r) names the row, from 7 at up to 8/7 to 2 at up to 4.
        let row = |r: f64| reducing_bank(r);
        assert_eq!(row(1.0), None, "a copy enlarges");
        assert_eq!(row(0.5), None);
        assert_eq!(row(1.14), Some(0));
        assert_eq!(row(1.15), Some(1));
        assert_eq!(row(1.6), Some(2), "8 / 1.6 is 5");
        assert_eq!(row(1.61), Some(3));
        assert_eq!(row(2.0), Some(3));
        assert_eq!(row(2.02), Some(4));
        assert_eq!(row(4.0), Some(5));
        assert_eq!(row(4.02), None, "past 4 the four taps again");
        // A sample on a pixel's centre, enlarging, is that pixel alone.
        let w = scaler_weights(10.5, 0.5, 0.0, 32.0, Filter::Smooth);
        let total: f64 = w.iter().map(|&(_, w)| w).sum();
        assert!((total - 1.0).abs() < 1e-12);
        assert_eq!(
            w.iter().find(|&&(i, _)| i == 10).map(|&(_, w)| w),
            Some(1.0)
        );
        assert!(w.iter().all(|&(i, w)| i == 10 || w == 0.0), "{w:?}");
        // A quarter past it and a quarter before it mirror each other.
        let after = scaler_weights(10.75, 0.5, 0.0, 32.0, Filter::Smooth);
        let before = scaler_weights(10.25, 0.5, 0.0, 32.0, Filter::Smooth);
        for &(i, w) in &after {
            let j = 20 - i;
            let m = before.iter().find(|&&(k, _)| k == j).map(|&(_, w)| w);
            assert_eq!(m, Some(w), "pixel {i} of {after:?} against {before:?}");
        }
        // Nine taps when shrinking, the edges repeated.
        let w = scaler_weights(0.5, 2.0, 0.0, 32.0, Filter::Smooth);
        assert_eq!(w.len(), 9);
        assert_eq!(w.iter().filter(|&&(i, _)| i == 0).count(), 5);
    }

    #[test]
    fn the_nearest_filter_filters_as_the_smooth_one() {
        let b = ScalerFormat::Bgra;
        let (l, size) = layout(b, 40, 30);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let mut src = vec![0u8; size];
        let picture = test_picture(40, 30);
        for y in 0..30 {
            for x in 0..40 {
                let p = picture.at(x, y);
                let at = y * of.planes[0].bytes_per_row + x * 4;
                src[at..at + 4].copy_from_slice(&[
                    (p[2] * 255.0) as u8,
                    (p[1] * 255.0) as u8,
                    (p[0] * 255.0) as u8,
                    255,
                ]);
            }
        }
        let out = |flags: u64, dw, dh| {
            let t = Transform {
                flags: TransformFlags(flags),
                ..Transform::default()
            };
            let p = plan(b, 40, 30, b, dw, dh, t);
            let (_, size) = layout(b, dw, dh);
            let mut dst = vec![0u8; size];
            assert_eq!(run(&p, &src, &mut dst), Ok(()));
            dst
        };
        let nearest = 0x80;
        assert_eq!(out(nearest, 120, 90), out(0, 120, 90), "up");
        assert_eq!(out(nearest, 20, 15), out(0, 20, 15), "down");
    }

    #[test]
    fn a_halving_filters_an_impulse_through_its_bank() {
        // A 32-wide 420v of luma 100 with one pixel at 140: halved, pixel 5
        // samples on the impulse at 10 and pixel 6 two pixels past it.
        let n = ScalerFormat::Nv12VideoRange;
        let (l, size) = layout(n, 32, 2);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let luma = of.planes[0];
        let mut src = vec![128u8; size];
        for y in 0..2 {
            let row = &mut src[luma.offset + y * luma.bytes_per_row..][..32];
            row.fill(100);
            row[10] = 140;
        }
        let p = plan(n, 32, 2, n, 16, 2, Transform::default());
        let (dl, size) = layout(n, 16, 2);
        let mut dst = vec![0u8; size];
        assert_eq!(run(&p, &src, &mut dst), Ok(()));
        let out = FormatOf::of(&dl).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let at = |x: usize| f64::from(dst[out.planes[0].offset + x]);
        let bank = &REDUCING[3];
        let total: f64 = (-4i32..=4)
            .map(|k| f64::from(bank[(32 * k).unsigned_abs() as usize]))
            .sum();
        let expect = |offset: usize| (100.0 + 40.0 * f64::from(bank[offset]) / total).round();
        assert_eq!(at(5), expect(0));
        assert_eq!(at(6), expect(64));
        assert_eq!(at(4), expect(64));
        assert!(at(5) >= 120.0, "half the impulse or more: {}", at(5));
    }

    /// The adaptive filter's choice by the steps about a sample point: an
    /// enlargement's by a step of 0.45 next to the point, or the drift of
    /// its largest step below it; a reduction's by how many steps there are
    #[test]
    fn the_adaptive_filter_moves_by_the_steps_about_the_point() {
        let up = |steps| adaptive_blend(steps, false);
        let down = |steps| adaptive_blend(steps, true);
        assert_eq!(up([0.0, 0.45, 0.0, 0.0, 0.0]), 0.6);
        assert_eq!(up([0.0, 0.0, 0.45, 0.45, 0.0]), 0.6);
        // Below the step: (255 m - 32) / 500, none under 32 codes, a
        // quarter at most, and a big step away from the point is drift.
        let drift = up([0.0, 0.0, 0.44, 0.0, 0.0]);
        assert!(
            (drift - (255.0 * 0.44 - 32.0) / 500.0).abs() < 1e-12,
            "{drift}"
        );
        assert_eq!(up([0.1, 0.0, 0.0, 0.1, 0.0]), 0.0);
        assert_eq!(up([0.9, 0.0, 0.0, 0.0, 0.0]), 0.25);
        // A reduction: a step at the point moves it 0.6, an impulse's two
        // 0.45, three none; a small step beside them stops it; one away
        // from the point moves nothing.
        assert_eq!(down([0.0, 0.5, 0.0, 0.0, 0.0]), 0.6);
        assert_eq!(down([0.0, 0.5, 0.5, 0.0, 0.0]), 0.45);
        assert_eq!(down([0.5, 0.5, 0.0, 0.5, 0.0]), 0.0);
        assert_eq!(down([0.0, 0.5, 0.0, 0.0, 0.06]), 0.0);
        assert_eq!(down([0.0, 0.5, 0.0, 0.0, 0.05]), 0.6);
        assert_eq!(down([0.0, 0.0, 0.9, 0.0, 0.0]), 0.0);
        assert_eq!(down([0.0, 0.44, 0.0, 0.0, 0.0]), 0.0);
    }

    /// An x420 luma impulse doubled with bit 12: from 461 codes high its
    /// neighbour moves 0.6 of the way from the taps' sum toward the
    /// bilinear sample, at 460 only by the drift
    #[test]
    fn a_sharp_impulse_takes_the_smoother_taps() {
        let x = ScalerFormat::P010VideoRange;
        let doubled = |height: f64| {
            let (l, size) = layout(x, 64, 2);
            let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
            let mut src = vec![0u8; size];
            for y in 0..2 {
                for i in 0..64 {
                    write_sample(&of, &mut src, 0, i, y, 0, 256.0);
                }
                write_sample(&of, &mut src, 0, 32, y, 0, 256.0 + height);
                for i in 0..32 {
                    write_sample(&of, &mut src, 1, i, 0, 0, 512.0);
                    write_sample(&of, &mut src, 1, i, 0, 1, 512.0);
                }
            }
            let t = Transform {
                flags: TransformFlags(0x1000),
                ..Transform::default()
            };
            let p = plan(x, 64, 2, x, 128, 2, t);
            let (dl, size) = layout(x, 128, 2);
            let dof = FormatOf::of(&dl).unwrap_or_else(|status| panic!("refused: {status:#x}"));
            let mut dst = vec![0u8; size];
            assert_eq!(run(&p, &src, &mut dst), Ok(()));
            read_sample(&dof, &dst, 0, 65, 0, 0)
        };
        // Pixel 65 samples at 32.25: ENLARGING's taps 444 and 108 of 512 on
        // pixels 32 and 33, the bilinear sample 0.75 of the impulse.
        let taps = 444.0 / 512.0;
        let at = |height: f64, beta: f64| (256.0 + height * (taps + beta * (0.75 - taps))).round();
        assert_eq!(doubled(461.0), at(461.0, 0.6));
        let drift = (255.0 * 460.0 / 1023.0 - 32.0) / 500.0;
        assert_eq!(doubled(460.0), at(460.0, drift));
    }

    /// A BGRA source moves each of R, G and B by its own steps, in light:
    /// a red impulse of 200 codes over black, 0.58 in light, doubled takes
    /// the smoother taps, and a green one of 16 beside it its own
    #[test]
    fn each_rgb_channel_takes_its_own_taps() {
        let b = ScalerFormat::Bgra;
        let (l, size) = layout(b, 32, 2);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let row = of.planes[0].bytes_per_row;
        let mut src = vec![0u8; size];
        for y in 0..2 {
            for x in 0..32 {
                src[y * row + x * 4 + 3] = 255;
            }
            src[y * row + 16 * 4 + 2] = 200;
            src[y * row + 16 * 4 + 1] = 16;
        }
        let t = Transform {
            flags: TransformFlags(0x1000),
            ..Transform::default()
        };
        let p = plan(b, 32, 2, b, 64, 2, t);
        let (_, size) = layout(b, 64, 2);
        let mut dst = vec![0u8; size];
        assert_eq!(run(&p, &src, &mut dst), Ok(()));
        let drow = 64 * 4;
        // Pixel 33 samples at 16.25, as in the impulse test above.
        let taps = 444.0 / 512.0;
        let light = |code: f64| srgb_to_linear(code / 255.0);
        let code = |l: f64| (linear_to_srgb(l) * 255.0).round();
        let moved = |beta: f64, l: f64| code(l * (taps + beta * (0.75 - taps)));
        let red = f64::from(dst[drow + 33 * 4 + 2]);
        let green = f64::from(dst[drow + 33 * 4 + 1]);
        assert_eq!(red, moved(0.6, light(200.0)));
        let small = light(16.0);
        let drift = ((255.0 * small - 32.0) / 500.0).clamp(0.0, 0.25);
        assert_eq!(green, moved(drift, small));
    }

    #[test]
    fn rgb_scales_in_linear_light() {
        // Columns of black and white, halved: about the mean of their
        // light, sRGB's 188, where the mean of their codes is 128. Into
        // 420v the same light, its luma that grey's, where scaling codes
        // would give 126.
        let b = ScalerFormat::Bgra;
        let (l, size) = layout(b, 32, 2);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let row = of.planes[0].bytes_per_row;
        let mut src = vec![0u8; size];
        for y in 0..2 {
            for x in 0..32 {
                let v = if x % 2 == 0 { 255 } else { 0 };
                src[y * row + x * 4..][..4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let p = plan(b, 32, 2, b, 16, 2, Transform::default());
        let (_, size) = layout(b, 16, 2);
        let mut dst = vec![0u8; size];
        assert_eq!(run(&p, &src, &mut dst), Ok(()));
        for x in 4..12 {
            let v = i32::from(dst[x * 4]);
            assert!((v - 188).abs() <= 3, "pixel {x}: {v}");
        }
        let grey = dst.clone();
        let n = ScalerFormat::Nv12VideoRange;
        let p = plan(b, 32, 2, n, 16, 2, Transform::default());
        let (dl, size) = layout(n, 16, 2);
        let mut dst = vec![0u8; size];
        assert_eq!(run(&p, &src, &mut dst), Ok(()));
        let luma = FormatOf::of(&dl).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        for x in 4..12 {
            let v = f64::from(dst[luma.planes[0].offset + x]);
            let at = 16.0 + 219.0 * f64::from(grey[x * 4]) / 255.0;
            assert!((v - at).abs() <= 1.0, "luma {x}: {v}, the grey's {at}");
        }
        // A black-to-white edge doubled rings past black and white in
        // light, which is clamped before it is converted: its luma stays
        // within 16 to 235.
        let (l, size) = layout(b, 16, 2);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let row = of.planes[0].bytes_per_row;
        let mut src = vec![0u8; size];
        for y in 0..2 {
            for x in 8..16 {
                src[y * row + x * 4..][..4].copy_from_slice(&[255; 4]);
            }
        }
        let t = Transform {
            flags: TransformFlags(0x1000),
            ..Transform::default()
        };
        let p = plan(b, 16, 2, n, 32, 2, t);
        let (dl, size) = layout(n, 32, 2);
        let mut dst = vec![0u8; size];
        assert_eq!(run(&p, &src, &mut dst), Ok(()));
        let luma = FormatOf::of(&dl).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let edge = &dst[luma.planes[0].offset..][..32];
        assert!(edge.iter().all(|&v| (16..=235).contains(&v)), "{edge:?}");
        assert_eq!((edge[0], edge[31]), (16, 235));
    }

    #[test]
    fn scales_sample_where_the_alignment_puts_them() {
        // A ramp in luma, which is scaled as its codes are.
        let n = ScalerFormat::Nv12VideoRange;
        let (l, size) = layout(n, 32, 4);
        let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
        let mut src = vec![128u8; size];
        for y in 0..4 {
            for x in 0..32 {
                src[of.planes[0].offset + y * of.planes[0].bytes_per_row + x] = (16 + 6 * x) as u8;
            }
        }
        let luma = |sw: u32, dw: u32, bits: u64, source: &[u8]| -> Vec<f64> {
            let t = Transform {
                flags: TransformFlags(bits),
                ..Transform::default()
            };
            let p = plan(n, sw, 4, n, dw, 4, t);
            let (dl, size) = layout(n, dw, 4);
            let mut dst = vec![0u8; size];
            assert_eq!(run(&p, source, &mut dst), Ok(()));
            let luma = FormatOf::of(&dl)
                .unwrap_or_else(|status| panic!("refused: {status:#x}"))
                .planes[0];
            (0..dw as usize)
                .map(|x| f64::from(dst[luma.offset + 2 * luma.bytes_per_row + x]))
                .collect()
        };
        // Doubled without bit 12: pixel d takes source d / 2.
        let up = luma(32, 64, 0, &src);
        for (d, v) in up.iter().enumerate().take(60).skip(4) {
            assert_eq!(*v, 16.0 + 6.0 * d as f64 / 2.0, "pixel {d} of {up:?}");
        }
        // With it: (d + 0.5) / 2 - 0.5, to within its rounding.
        let fixed = luma(32, 64, 0x1000, &src);
        for (d, v) in fixed.iter().enumerate().take(60).skip(4) {
            let s = (d as f64 + 0.5) / 2.0 - 0.5;
            assert!(
                (*v - (16.0 + 6.0 * s)).abs() <= 0.5 + 1e-9,
                "pixel {d} of {fixed:?}"
            );
        }
        // Halved, with bit 12 or not: pixel d takes source 2d, its nine
        // taps about it, which keep a ramp, away from the edges.
        for bits in [0, 0x1000] {
            let down = luma(32, 16, bits, &src);
            for (d, v) in down.iter().enumerate().take(14).skip(2) {
                assert_eq!(*v, 16.0 + 12.0 * d as f64, "pixel {d} of {down:?}");
            }
        }
    }

    /// The taps a plan's pixels take, Apple's filter's: a halving takes
    /// [`REDUCING`]'s row 3 at whole pixels about `2d`, its window's edge
    /// pixel repeated past it; an axis that keeps its size takes the pixel
    /// alone; a doubling with bit 12 takes [`ENLARGING`]'s four about its
    /// sample point; and a quarter turn counts its turned source's pixels,
    /// its height across.
    #[test]
    fn taps_are_the_turned_sources_pixels_and_weights() {
        let t = |bits: u64, fix: bool| Transform {
            flags: TransformFlags(bits | if fix { 0x1000 } else { 0 }),
            ..Transform::default()
        };
        let bgra = ScalerFormat::Bgra;
        let half = taps(&plan(bgra, 32, 8, bgra, 16, 8, t(0, true)));
        assert_eq!(half.turned, (32, 8));
        let bank = [0.0, -32.0, 0.0, 156.0, 260.0, 156.0, 0.0, -32.0, 0.0];
        let expected: Vec<(usize, f64)> = (2..=10).zip(bank.map(|w| w / 508.0)).collect();
        assert_eq!(half.across[3], expected);
        assert!(half.across[0][..5].iter().all(|&(i, _)| i == 0));
        assert_eq!(half.down[2], vec![(1, 0.0), (2, 1.0), (3, 0.0), (4, 0.0)]);
        // Doubled with bit 12: pixel 3 samples at 1.25 (40 32nds).
        let double = taps(&plan(bgra, 16, 8, bgra, 32, 8, t(0, true)));
        assert_eq!(
            double.across[3],
            vec![
                (0, -0.0625),
                (1, 0.867_187_5),
                (2, 0.210_937_5),
                (3, -0.015_625)
            ]
        );
        let turned = taps(&plan(bgra, 32, 8, bgra, 8, 32, t(4, false)));
        assert_eq!(turned.turned, (8, 32));
        assert_eq!((turned.across.len(), turned.down.len()), (8, 32));
    }

    #[test]
    fn subsampled_chroma_keeps_half_the_lumas_offset() {
        // A ramp in Cb along one axis, doubled with bit 12 or without it,
        // read back from the destination's Cb along that axis.
        let cb = |format: ScalerFormat, down: bool, bits: u64, step: f64| -> Vec<f64> {
            let (sw, sh, dw, dh) = if down {
                (8, 32, 16, 64)
            } else {
                (32, 8, 64, 16)
            };
            let (l, size) = layout(format, sw, sh);
            let of = FormatOf::of(&l).unwrap_or_else(|status| panic!("refused: {status:#x}"));
            let packed = !matches!(format.packing(), Packing::Biplanar420);
            let (plane, cb, cr) = if packed { (0, 1, 3) } else { (1, 0, 1) };
            let mut src = vec![128u8; size];
            let (cw, ch) = (sw as usize / 2, if packed { sh } else { sh / 2 } as usize);
            for y in 0..ch {
                for x in 0..cw {
                    let i = if down { y } else { x };
                    write_sample(&of, &mut src, plane, x, y, cb, 16.0 + step * i as f64);
                    write_sample(&of, &mut src, plane, x, y, cr, 128.0);
                }
            }
            let t = Transform {
                flags: TransformFlags(bits),
                ..Transform::default()
            };
            let p = plan(format, sw, sh, format, dw, dh, t);
            let (dl, size) = layout(format, dw, dh);
            let dof = FormatOf::of(&dl).unwrap_or_else(|status| panic!("refused: {status:#x}"));
            let mut dst = vec![0u8; size];
            assert_eq!(run(&p, &src, &mut dst), Ok(()));
            let (dcw, dch) = (dw as usize / 2, if packed { dh } else { dh / 2 } as usize);
            if down {
                (0..dch)
                    .map(|y| read_sample(&dof, &dst, plane, 2, y, cb))
                    .collect()
            } else {
                (0..dcw)
                    .map(|x| read_sample(&dof, &dst, plane, x, 2, cb))
                    .collect()
            }
        };
        // 4:2:0, across and down: chroma sample k sits on luma pixel 2k
        // ([`decode_turned`], [`Siting::APPLE`]), so Cb at luma position
        // p is 16 + 7p. Doubled with bit 12, the destination's k takes
        // (2k + 0.25) / 2 - 0.25, half the luma's offset of -0.25 a pixel,
        // so 16 + 7k - 0.875, where the luma's place gives 16 + 7k - 1.75.
        let n = ScalerFormat::Nv12VideoRange;
        for down in [false, true] {
            let fixed = cb(n, down, 0x1000, 14.0);
            for (k, v) in fixed.iter().enumerate().take(29).skip(2) {
                let at = 16.0 + 7.0 * k as f64 - 0.875;
                assert!((v - at).abs() <= 0.5, "sample {k} of {fixed:?}");
            }
            // Without bit 12, first pixels: k takes luma pixel k.
            let first = cb(n, down, 0, 14.0);
            for (k, v) in first.iter().enumerate().take(29).skip(2) {
                assert_eq!(*v, 16.0 + 7.0 * k as f64, "sample {k} of {first:?}");
            }
        }
        // 4:2:2 down, which it does not subsample: the luma's place, row k
        // taking (k + 0.5) / 2 - 0.5, so 16 + 4k - 2 for a ramp of 8 a row.
        let fixed = cb(ScalerFormat::Yuyv, true, 0x1000, 8.0);
        for (k, v) in fixed.iter().enumerate().take(60).skip(4) {
            assert_eq!(*v, 16.0 + 4.0 * k as f64 - 2.0, "row {k} of {fixed:?}");
        }
    }

    #[test]
    fn halving_averages_and_doubling_interpolates() {
        let (_, src) = surface(ScalerFormat::Bgra, 64, 64, &test_picture(64, 64));
        let down = plan(
            ScalerFormat::Bgra,
            64,
            64,
            ScalerFormat::Bgra,
            32,
            32,
            Transform::default(),
        );
        let (_, size) = layout(ScalerFormat::Bgra, 32, 32);
        let mut small = vec![0u8; size];
        assert_eq!(run(&down, &src, &mut small), Ok(()));
        let up = plan(
            ScalerFormat::Bgra,
            32,
            32,
            ScalerFormat::Bgra,
            64,
            64,
            Transform::default(),
        );
        let mut big = vec![0u8; src.len()];
        assert_eq!(run(&up, &small, &mut big), Ok(()));
        let (of, _) = surface(ScalerFormat::Bgra, 64, 64, &flat_picture(1, 1, [0.0; 4]));
        let v = psnr(&of, &big, &src);
        assert!(v[0] > 25.0, "{v:?}");
        let _ = FourCc::new(*b"BGRA");
    }
}
