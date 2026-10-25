//! Where a scaler takes 4:2:0 and 4:2:2 chroma to sit when it converts to
//! RGB, and where it puts it when it converts from RGB: the chroma kernels
//! of an [`Accelerator`], measured as they were on Apple's scaler. Ours.
//!
//! Up: a 64x64 source of constant luma whose Cb is raised in every eighth
//! chroma column and whose Cr is raised in every eighth chroma row (4:2:0),
//! converted 1:1 to BGRA. B is luma plus a multiple of Cb and R luma plus a
//! multiple of Cr whatever the matrix, so B's excess along a row is a Cb
//! column's kernel over the luma columns, and R's down a column a Cr row's.
//!
//! Down: a 64x64 BGRA source, grey but for the luma columns 8m + k with Cb
//! raised (the rows, with Cr), one transform for each k of 0 to 7,
//! converted 1:1 to Y'CbCr with the destination's siting in the transform's
//! flags (bits 15 to 17). From the eight, each chroma sample's weights over
//! the luma columns (rows) near it are its kernel.

use reims_vgpu_video_core::device_wait::GuestHold;
use reims_vgpu_video_core::scaler::backend::{Accelerator, TransformJob};
use reims_vgpu_video_core::scaler::format::{Packing, ScalerFormat};
use reims_vgpu_video_core::scaler::io_return::{self, IoReturn};
use reims_vgpu_video_core::scaler::plan::{FormatOf, Matrix, Plan, Siting};
use reims_vgpu_video_core::scaler::reference::{self, Picture, Space};
use reims_vgpu_video_core::scaler::request::{Transform, TransformFlags};
use reims_vgpu_video_core::surfaces::GuestSurface;

use super::cases::{Memory, layout, length};

/// The side of the measured surfaces.
const SIZE: usize = 64;
/// How far apart the raised lines are, in chroma samples (up) or luma
/// pixels (down).
const SPACING: usize = 8;
/// How much a raised line's chroma is raised.
const RAISED: f64 = 0.18;

/// A chroma sample's kernel: its weights over the luma columns (rows) from
/// its first less 2 to its first plus 3, summing to 1, and their centroid
/// less its first, where the scaler takes or puts the sample: 0 co-sited
/// (left or top), 0.5 centred, 1 bottom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Kernel {
    /// The weights.
    pub weights: [f64; 6],
    /// Their centroid.
    pub site: f64,
}

impl Kernel {
    fn of(sums: [f64; 6]) -> Kernel {
        let total: f64 = sums.iter().sum();
        let moment: f64 = sums
            .iter()
            .enumerate()
            .map(|(i, w)| (i as f64 - 2.0) * w)
            .sum();
        Kernel {
            weights: sums.map(|w| if total == 0.0 { 0.0 } else { w / total }),
            site: if total == 0.0 {
                f64::NAN
            } else {
                moment / total
            },
        }
    }

    /// Whether every weight is within `tolerance` of `weights`.
    pub fn near(&self, weights: [f64; 6], tolerance: f64) -> bool {
        self.weights
            .iter()
            .zip(weights)
            .all(|(a, b)| (a - b).abs() <= tolerance)
    }
}

/// One measured kernel.
#[derive(Clone, Debug)]
pub struct Measured {
    /// `up` (Y'CbCr into BGRA) or `down` (BGRA into Y'CbCr).
    pub direction: &'static str,
    /// The Y'CbCr side's format.
    pub format: ScalerFormat,
    /// Along the rows (false) or the columns (true).
    pub vertical: bool,
    /// The destination's siting, flag bits 15 to 17.
    pub cosite: u32,
    /// The kernel, or the transform's status where it failed.
    pub kernel: Result<Kernel, IoReturn>,
}

impl Measured {
    /// The line `check` prints.
    pub fn line(&self) -> String {
        let what = format!(
            "siting {} {} {} cosite {}",
            self.direction,
            super::cases::name(self.format),
            if self.vertical { 'v' } else { 'h' },
            self.cosite
        );
        match self.kernel {
            Ok(k) => format!(
                "{what}: site {:+.2} kernel {}",
                k.site,
                k.weights
                    .iter()
                    .map(|w| format!("{w:.2}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Err(status) => format!("{what}: status {status:#x}"),
        }
    }
}

/// A surface of `format` holding `picture` in `space`.
fn surface(format: ScalerFormat, picture: &Picture, space: Space) -> Memory {
    let layout = layout(format, SIZE as u32, SIZE as u32);
    let mut bytes = vec![0u8; length(&layout)];
    if let Ok(of) = FormatOf::of(&layout) {
        reference::encode(
            &of,
            &mut bytes,
            picture,
            0,
            0,
            space,
            Matrix::Bt601,
            Siting::CENTRED,
        );
    }
    Memory::new(layout, bytes)
}

/// Runs a 1:1 transform of `source` into a new surface of `format` with
/// `cosite` in its flags: the destination's format and bytes.
fn convert(
    accelerator: &mut dyn Accelerator,
    source: &Memory,
    format: ScalerFormat,
    cosite: u32,
) -> Result<(FormatOf, Vec<u8>), IoReturn> {
    let grey = reference::flat_picture(SIZE, SIZE, [0.5, 0.5, 0.5, 1.0]);
    let destination = surface(format, &grey, Space::Rgb);
    let transform = Transform {
        flags: TransformFlags(u64::from(cosite) << 15),
        ..Transform::default()
    };
    let plan = Plan::new(&transform, &source.layout(), &destination.layout())?;
    let of = plan.destination_format.clone();
    let mut job = TransformJob {
        transform: &transform,
        plan,
        source,
        destination: &destination,
        hold: GuestHold::nothing(),
        inputs: Default::default(),
        outputs: Default::default(),
    };
    let status = accelerator.transform(&mut job);
    if status != io_return::SUCCESS {
        return Err(status);
    }
    Ok((of, destination.bytes()))
}

/// Byte `component` of pixel or pair `x` of row `y` of plane `plane`.
fn byte(of: &FormatOf, bytes: &[u8], plane: usize, x: usize, y: usize, component: usize) -> f64 {
    let span = &of.planes[plane];
    let size = match of.format.packing() {
        Packing::Biplanar420 => {
            if plane == 0 {
                1
            } else {
                2
            }
        }
        _ => 4,
    };
    f64::from(bytes[span.offset + y * span.bytes_per_row + x * size + component])
}

/// The up kernels of `format` into BGRA: along the rows, and for 4:2:0
/// down the columns.
fn up(accelerator: &mut dyn Accelerator, format: ScalerFormat) -> Vec<Measured> {
    let biplanar = matches!(format.packing(), Packing::Biplanar420);
    let mut picture = reference::flat_picture(SIZE, SIZE, [0.5, 0.0, 0.0, 1.0]);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let p = &mut picture.pixels[y * SIZE + x];
            if (x / 2) % SPACING == 3 {
                p[1] = RAISED;
            }
            if biplanar && (y / 2) % SPACING == 3 {
                p[2] = RAISED;
            }
        }
    }
    let source = surface(format, &picture, Space::YCbCr);
    let result = convert(accelerator, &source, ScalerFormat::Bgra, 0);
    let axes = if biplanar { 2 } else { 1 };
    (0..axes)
        .map(|axis| {
            let kernel = result.as_ref().map_err(|s| *s).map(|(of, bytes)| {
                // B is byte 0 of BGRA, R byte 2.
                let channel = if axis == 0 { 0 } else { 2 };
                let at = |i: usize, l: usize| {
                    let (x, y) = if axis == 0 { (i, l) } else { (l, i) };
                    byte(of, bytes, 0, x, y, channel)
                };
                let background: f64 = (0..SIZE).map(|l| at(0, l)).sum::<f64>() / SIZE as f64;
                let mut sums = [0.0; 6];
                // Chroma samples 11 and 19, whose first luma pixels are 22
                // and 38.
                for first in [22, 38] {
                    for (k, sum) in sums.iter_mut().enumerate() {
                        *sum += (0..SIZE)
                            .map(|l| at(first + k - 2, l) - background)
                            .sum::<f64>();
                    }
                }
                Kernel::of(sums)
            });
            Measured {
                direction: "up",
                format,
                vertical: axis == 1,
                cosite: 0,
                kernel,
            }
        })
        .collect()
}

/// The down kernels of BGRA into `format` with `cosite`.
fn down(accelerator: &mut dyn Accelerator, format: ScalerFormat, cosite: u32) -> Vec<Measured> {
    let biplanar = matches!(format.packing(), Packing::Biplanar420);
    let axes = if biplanar { 2 } else { 1 };
    (0..axes)
        .map(|axis| {
            let mut sums = [0.0; 6];
            let mut failed = None;
            for k in 0..SPACING {
                let raised = if axis == 0 {
                    reference::to_rgb([0.5, RAISED, 0.0, 1.0], Matrix::Bt601)
                } else {
                    reference::to_rgb([0.5, 0.0, RAISED, 1.0], Matrix::Bt601)
                };
                let mut picture = reference::flat_picture(SIZE, SIZE, [0.5, 0.5, 0.5, 1.0]);
                for y in 0..SIZE {
                    for x in 0..SIZE {
                        if (if axis == 0 { x } else { y }) % SPACING == k {
                            picture.pixels[y * SIZE + x] = raised;
                        }
                    }
                }
                let source = surface(ScalerFormat::Bgra, &picture, Space::Rgb);
                let (of, bytes) = match convert(accelerator, &source, format, cosite) {
                    Ok(out) => out,
                    Err(status) => {
                        failed = Some(status);
                        break;
                    }
                };
                // Cb is byte 0 of a 4:2:0 chroma pair, Cr byte 1; of a 4:2:2
                // pair, Cb is at its packing's offset 1.
                let component = match format.packing() {
                    Packing::Packed422(offsets) => offsets[1],
                    _ => axis,
                };
                let plane = usize::from(biplanar);
                let lines = if biplanar { SIZE / 2 } else { SIZE };
                for c in 8..24 {
                    for m in 0..SIZE / SPACING {
                        let offset = (SPACING * m + k) as isize - 2 * c as isize;
                        if !(-2..=3).contains(&offset) {
                            continue;
                        }
                        let sum: f64 = (0..lines)
                            .map(|l| {
                                let (x, y) = if axis == 0 { (c, l) } else { (l, c) };
                                byte(&of, &bytes, plane, x, y, component) - 128.0
                            })
                            .sum();
                        sums[(offset + 2) as usize] += sum;
                    }
                }
            }
            Measured {
                direction: "down",
                format,
                vertical: axis == 1,
                cosite,
                kernel: failed.map_or_else(|| Ok(Kernel::of(sums)), Err),
            }
        })
        .collect()
}

/// Every kernel `accelerator` gives: up from 420v, 420f and yuvs; down into
/// 420v with each siting of none (0) to bottom (6), and into yuvs with none.
/// 2vuy is left out: the plan refuses it either way, as Apple's scaler does.
pub fn measure(accelerator: &mut dyn Accelerator) -> Vec<Measured> {
    use ScalerFormat::*;
    let mut all = Vec::new();
    for format in [Nv12VideoRange, Nv12FullRange, Yuyv] {
        all.extend(up(accelerator, format));
    }
    for cosite in 0..7 {
        all.extend(down(accelerator, Nv12VideoRange, cosite));
    }
    all.extend(down(accelerator, Yuyv, 0));
    all
}
