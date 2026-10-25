//! Where a scaler takes each destination pixel from when it scales: the
//! `map` lines, measured as Apple's scaler was. Ours.
//!
//! The source rises by 6 a pixel over teeth of 32 pixels along one axis,
//! and falls so in a second channel or transform: BGRA's red rises and its
//! green falls; 420v's luma rises in one transform and falls in another.
//! Inside a tooth, a filter that keeps a straight line straight gives back
//! the source position each destination sample was taken at, and the mean
//! of the rising and the falling reading takes out the rounding of a value
//! between two. Away from the teeth's edges and the source window's, those
//! positions less where the plan's alignment puts them are given as a mean
//! and its spread, beside the same for every axis keeping pixel centres
//! and for every axis putting the first pixels on each other.

use reims_vgpu_video_core::device_wait::GuestHold;
use reims_vgpu_video_core::scaler::backend::{Accelerator, TransformJob};
use reims_vgpu_video_core::scaler::format::ScalerFormat;
use reims_vgpu_video_core::scaler::io_return::{self, IoReturn};
use reims_vgpu_video_core::scaler::plan::{Alignment, Plan};
use reims_vgpu_video_core::scaler::request::{
    FIXED_ONE, FixedRect, Rect, Transform, TransformFlags,
};
use reims_vgpu_video_core::surfaces::GuestSurface;

use super::cases::{Memory, layout, length};

/// What draws a source: its byte at pixel or pair (x, y) of a plane, and a
/// component.
type Draw = dyn Fn(usize, usize, usize, usize) -> u8;

/// A tooth's length, in source pixels.
const TOOTH: f64 = 32.0;
/// How much a tooth rises a pixel.
const RISE: f64 = 6.0;
/// Where a tooth starts.
const FOOT: f64 = 24.0;

fn tooth(i: usize) -> u8 {
    (FOOT + RISE * (i % TOOTH as usize) as f64) as u8
}

fn fall(i: usize) -> u8 {
    (FOOT + RISE * (TOOTH as usize - 1 - i % TOOTH as usize) as f64) as u8
}

/// One case: a format, the two surfaces' sizes, the windows (whole where
/// zero), the `Transform` bits, and whether `FixUpscaling` is set, as
/// every `TransformSurface` sets it and `TransferSurface` does not.
#[derive(Clone, Copy, Debug)]
struct Case {
    format: ScalerFormat,
    source: (u32, u32),
    destination: (u32, u32),
    window: (f64, f64, f64, f64),
    rect: (u32, u32, u32, u32),
    bits: u32,
    fix: bool,
}

/// One axis's measurement.
#[derive(Clone, Debug)]
pub struct Mapped {
    /// What was run, as the line names it.
    pub name: String,
    /// `x` or `y`, of the source.
    pub axis: char,
    /// The samples measured.
    pub samples: usize,
    /// The positions less the plan's alignment's: mean and spread; less
    /// pixel centres kept; less first pixels on each other.
    pub less: [(f64, f64); 3],
    /// Or the transform's status, where it failed.
    pub status: Option<IoReturn>,
}

impl Mapped {
    /// The line `check` prints.
    pub fn line(&self) -> String {
        if let Some(status) = self.status {
            return format!("map {} {}: status {status:#x}", self.name, self.axis);
        }
        let [plan, centres, first] = self.less;
        format!(
            "map {} {}: n {} less plan {:+.3} ({:.3}), centres {:+.3} ({:.3}), first pixels {:+.3} ({:.3})",
            self.name,
            self.axis,
            self.samples,
            plan.0,
            plan.1,
            centres.0,
            centres.1,
            first.0,
            first.1
        )
    }
}

fn cases() -> Vec<Case> {
    use ScalerFormat::{Bgra, Nv12VideoRange};
    let whole = |format, source, destination, bits, fix| Case {
        format,
        source,
        destination,
        window: (0.0, 0.0, 0.0, 0.0),
        rect: (0, 0, 0, 0),
        bits,
        fix,
    };
    let mut list = Vec::new();
    for fix in [false, true] {
        for format in [Bgra, Nv12VideoRange] {
            for (source, destination) in [
                ((32, 32), (64, 64)),
                ((64, 64), (96, 96)),
                ((40, 40), (100, 100)),
                ((32, 32), (256, 256)),
                ((64, 64), (32, 32)),
                ((96, 96), (64, 64)),
                ((256, 256), (64, 64)),
                ((64, 32), (32, 64)),
            ] {
                list.push(whole(format, source, destination, 0, fix));
            }
        }
        list.push(whole(Bgra, (32, 48), (96, 64), 4, fix));
        list.push(whole(Bgra, (32, 32), (64, 64), 2, fix));
        list.push(Case {
            window: (10.0, 6.0, 64.0, 64.0),
            rect: (8, 4, 32, 32),
            ..whole(Bgra, (128, 128), (48, 48), 0, fix)
        });
        list.push(Case {
            window: (10.0, 6.0, 16.0, 16.0),
            rect: (8, 4, 32, 32),
            ..whole(Bgra, (64, 64), (48, 48), 0, fix)
        });
        // Fractional windows: where a backend takes the fraction.
        list.push(Case {
            window: (10.5, 6.25, 64.0, 64.0),
            rect: (8, 4, 32, 32),
            ..whole(Bgra, (128, 128), (48, 48), 0, fix)
        });
        list.push(Case {
            window: (10.5, 6.25, 16.0, 16.0),
            rect: (8, 4, 32, 32),
            ..whole(Bgra, (64, 64), (48, 48), 0, fix)
        });
    }
    list
}

/// A sample: its destination pixel, its value, and where each rule takes
/// it from (the plan's, centres, first pixels).
struct Sample {
    at: (f64, f64),
    rise: f64,
    fall: f64,
    rules: [f64; 3],
}

/// Least squares of `s = a x + b y + c`.
fn fit(points: &[((f64, f64), f64)]) -> Option<[f64; 3]> {
    let mut m = [[0.0f64; 3]; 3];
    let mut v = [0.0f64; 3];
    for &((x, y), s) in points {
        let q = [x, y, 1.0];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += q[i] * q[j];
            }
            v[i] += q[i] * s;
        }
    }
    let det = |m: &[[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(&m);
    if d.abs() < 1e-9 {
        // A degenerate axis (every sample in one destination column, say):
        // a constant.
        let n = points.len() as f64;
        return (n > 0.0).then(|| [0.0, 0.0, points.iter().map(|p| p.1).sum::<f64>() / n]);
    }
    let mut out = [0.0; 3];
    for (k, o) in out.iter_mut().enumerate() {
        let mut t = m;
        for i in 0..3 {
            t[i][k] = v[i];
        }
        *o = det(&t) / d;
    }
    Some(out)
}

/// The samples' positions, unwrapped from their teeth first by the plan's
/// rule over the first tooth and a half, then by the fit of the pass
/// before, less each rule's: (count, [(mean, spread); 3]).
fn measure(samples: &[Sample], lo: f64, hi: f64, margin: f64) -> (usize, [(f64, f64); 3]) {
    let mut line: Option<[f64; 3]> = None;
    let mut taken = Vec::new();
    for _ in 0..4 {
        taken.clear();
        for p in samples {
            let expected = match line {
                Some([a, b, c]) => a * p.at.0 + b * p.at.1 + c,
                None if p.rules[0] > 1.5 * TOOTH => continue,
                None => p.rules[0],
            };
            if expected < lo + margin || expected > hi - 1.0 - margin {
                continue;
            }
            let phase = expected.rem_euclid(TOOTH);
            if phase < margin || phase > TOOTH - 1.0 - margin {
                continue;
            }
            let rising = (p.rise - FOOT) / RISE;
            let falling = TOOTH - 1.0 - (p.fall - FOOT) / RISE;
            let s = TOOTH * (expected / TOOTH).floor() + (rising + falling) / 2.0;
            taken.push((p, s));
        }
        let points: Vec<_> = taken.iter().map(|(p, s)| (p.at, *s)).collect();
        match fit(&points) {
            Some(f) if points.len() >= 8 => line = Some(f),
            _ => return (0, [(f64::NAN, f64::NAN); 3]),
        }
    }
    let n = taken.len() as f64;
    let less = std::array::from_fn(|k| {
        let d: Vec<f64> = taken.iter().map(|(p, s)| s - p.rules[k]).collect();
        let mean = d.iter().sum::<f64>() / n;
        let spread = (d.iter().map(|e| (e - mean) * (e - mean)).sum::<f64>() / n).sqrt();
        (mean, spread)
    });
    (taken.len(), less)
}

/// Where each rule takes destination pixel `(dx, dy)` from, in source
/// pixels (pixel i's centre at i): the plan's alignment, centres both ways,
/// first pixels both ways.
fn rules(plan: &Plan, dx: f64, dy: f64) -> [(f64, f64); 3] {
    let fixed = |v: u64| v as f64 / FIXED_ONE as f64;
    let r = plan.source_rect;
    let (wx, wy, ww, wh) = (fixed(r.x), fixed(r.y), fixed(r.width), fixed(r.height));
    let d = plan.destination_rect;
    let (rx, ry) = plan.ratios();
    let (u, v) = (dx - f64::from(d.x), dy - f64::from(d.y));
    let at = |alignment: [Alignment; 2]| {
        let (tu, tv) = (
            alignment[0].source_index(u, rx) + 0.5,
            alignment[1].source_index(v, ry) + 0.5,
        );
        let (sx, sy) = plan.orientation.source_point(tu, tv, ww, wh);
        (wx + sx - 0.5, wy + sy - 0.5)
    };
    [
        at(plan.alignment),
        at([Alignment::Centres; 2]),
        at([Alignment::FirstPixels; 2]),
    ]
}

/// Runs one case's transform of a source drawn by `draw` (x, y, plane,
/// component to byte): the plan and the destination's bytes.
fn run(
    accelerator: &mut dyn Accelerator,
    case: &Case,
    draw: &Draw,
) -> Result<(Plan, Vec<u8>), IoReturn> {
    let fill = |size: (u32, u32), draw: &Draw| {
        let l = layout(case.format, size.0, size.1);
        let mut bytes = vec![0u8; length(&l)];
        for (plane, span) in l.planes.iter().enumerate() {
            let per = if case.format == ScalerFormat::Bgra || plane == 1 {
                if plane == 1 { 2 } else { 4 }
            } else {
                1
            };
            for y in 0..span.height as usize {
                for x in 0..span.width as usize {
                    for c in 0..per {
                        bytes[span.offset + y * span.bytes_per_row + x * per + c] =
                            draw(x, y, plane, c);
                    }
                }
            }
        }
        Memory::new(l, bytes)
    };
    let source = fill(case.source, draw);
    let destination = fill(case.destination, &|_, _, plane, c| match (plane, c) {
        (_, 3) => 255,
        _ => 128,
    });
    let q = |v: f64| (v * FIXED_ONE as f64).round() as u64;
    let transform = Transform {
        source_rect: FixedRect {
            x: q(case.window.0),
            y: q(case.window.1),
            width: q(case.window.2),
            height: q(case.window.3),
        },
        destination_rect: Rect {
            x: case.rect.0,
            y: case.rect.1,
            width: case.rect.2,
            height: case.rect.3,
        },
        flags: TransformFlags(u64::from(case.bits) | if case.fix { 0x1000 } else { 0 }),
        ..Transform::default()
    };
    let plan = Plan::new(&transform, &source.layout(), &destination.layout())?;
    let mut job = TransformJob {
        transform: &transform,
        plan: plan.clone(),
        source: &source,
        destination: &destination,
        hold: GuestHold::nothing(),
        inputs: Default::default(),
        outputs: Default::default(),
    };
    let status = accelerator.transform(&mut job);
    if status != io_return::SUCCESS {
        return Err(status);
    }
    Ok((plan, destination.bytes()))
}

fn name(case: &Case) -> String {
    let mut name = format!(
        "{} {} {}x{}->{}x{} t{}",
        if case.fix { "transform" } else { "transfer" },
        super::cases::name(case.format),
        case.source.0,
        case.source.1,
        case.destination.0,
        case.destination.1,
        case.bits
    );
    if case.window.2 != 0.0 {
        let (x, y, w, h) = case.window;
        let (rx, ry, rw, rh) = case.rect;
        name.push_str(&format!(" ({x},{y} {w}x{h})->({rx},{ry} {rw}x{rh})"));
    }
    name
}

/// One case's axes: for each, BGRA's red rising and green falling in one
/// transform, or 420v's luma rising in one and falling in another.
fn case(accelerator: &mut dyn Accelerator, case: &Case) -> Vec<Mapped> {
    let nv12 = case.format != ScalerFormat::Bgra;
    let mut out = Vec::new();
    for axis in ['x', 'y'] {
        let along = move |x: usize, y: usize| if axis == 'x' { x } else { y };
        let runs: Vec<Box<Draw>> = if nv12 {
            vec![
                Box::new(move |x, y, plane, _| if plane == 0 { tooth(along(x, y)) } else { 128 }),
                Box::new(move |x, y, plane, _| if plane == 0 { fall(along(x, y)) } else { 128 }),
            ]
        } else {
            vec![Box::new(move |x, y, _, c| match c {
                1 => fall(along(x, y)),
                2 => tooth(along(x, y)),
                3 => 255,
                _ => 128,
            })]
        };
        let mut results = Vec::new();
        for draw in &runs {
            match run(accelerator, case, draw.as_ref()) {
                Ok(result) => results.push(result),
                Err(status) => {
                    out.push(Mapped {
                        name: name(case),
                        axis,
                        samples: 0,
                        less: [(f64::NAN, f64::NAN); 3],
                        status: Some(status),
                    });
                    break;
                }
            }
        }
        if results.len() != runs.len() {
            continue;
        }
        let plan = &results[0].0;
        let span = &plan.destination_format.planes[0];
        let d = plan.destination_rect;
        let mut samples = Vec::new();
        for y in d.y as usize..(d.y + d.height) as usize {
            for x in d.x as usize..(d.x + d.width) as usize {
                let row = span.offset + y * span.bytes_per_row;
                let (rise, fall) = if nv12 {
                    (results[0].1[row + x], results[1].1[row + x])
                } else {
                    (results[0].1[row + x * 4 + 2], results[0].1[row + x * 4 + 1])
                };
                let pick = |p: (f64, f64)| if axis == 'x' { p.0 } else { p.1 };
                samples.push(Sample {
                    at: (x as f64, y as f64),
                    rise: f64::from(rise),
                    fall: f64::from(fall),
                    rules: rules(plan, x as f64, y as f64).map(pick),
                });
            }
        }
        // The ratio along this source axis, after the turn.
        let (rx, ry) = plan.ratios();
        let ratio = if (axis == 'x') != plan.orientation.rotates() {
            rx
        } else {
            ry
        };
        let margin = 3.0 + ratio.max(1.0).ceil() + (ratio - 1.0).abs() / 2.0;
        let fixed = |v: u64| v as f64 / FIXED_ONE as f64;
        let r = plan.source_rect;
        let (lo, len) = if axis == 'x' {
            (fixed(r.x), fixed(r.width))
        } else {
            (fixed(r.y), fixed(r.height))
        };
        let (n, less) = measure(&samples, lo, lo + len, margin);
        out.push(Mapped {
            name: name(case),
            axis,
            samples: n,
            less,
            status: None,
        });
    }
    out
}

/// Every case's axes, as `accelerator` scales them.
pub fn measure_all(accelerator: &mut dyn Accelerator) -> Vec<Mapped> {
    cases().iter().flat_map(|c| case(accelerator, c)).collect()
}
