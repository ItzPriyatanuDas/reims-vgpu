//! Core's motion-estimation part on NVENC's ME-only mode
//! ([`crate::nvenc::motion`]). What it converts, since NVENC's vectors
//! are not Apple's:
//! - the blocks: one H.264 macroblock's four 8x8 vectors become four
//!   blocks of core's field, which core puts on Apple's grid
//!   (`motion::resample`);
//! - the unit: NVENC's quarter pixels become pixels, the quarter kept,
//!   which Apple's half floats hold exactly;
//! - the size: NVENC takes sizes from its minimum, even ones, where Apple's
//!   processor takes any from 2x2, so a picture is estimated inside a frame
//!   of at least NVENC's size, its last column and row repeated into the
//!   rest, and the vectors of the picture's blocks alone are kept.
//!
//! The sign is the same: an H.264 vector points from the current block to
//! where it is in the reference, as Apple's does.

use std::sync::Arc;

use reims_vgpu_video_core::backend::{BackendError, PixelFormat};
use reims_vgpu_video_core::motion::{
    MAX_SIDE, MotionEstimationBackend, MotionEstimationCapabilities, MotionEstimator,
    MotionEstimatorConfig, MotionField, MotionPicture, MotionVector,
};
use reims_vgpu_video_core::surfaces::PlaneSource;

use crate::cuda::Gpu;
use crate::error::Error;
use crate::nvenc::motion::{MotionLimits, Nv12, NvencMotion, motion_limits};
use crate::nvenc::motion_ffi::{NV_ENC_H264_MV_DATA, NV_ENC_MVECTOR};
use crate::usize_of;

/// The block size of the field: H.264's 8x8 partitions.
const BLOCK: u32 = 8;
/// The preset every session searches at, true motion or not: NVENC's
/// widest search. On an RTX 5080 Laptop GPU (driver 615.71.09,
/// 2026-10-24, `gpu::measure_presets`), P5 to P7 found the same vectors, more of a
/// large shift on a small picture than P4 (62.5% of the cells of 256x128
/// moved by (20, -12), against 46.1%), in the same time: 2.0 ms an
/// estimate at 720p, 4.4 at 1080p and 17.2 at 4K, copies included.
const PRESET: u8 = 7;

/// Core's motion part on one GPU's NVENC.
pub struct NvidiaMotion {
    gpu: Arc<Gpu>,
    limits: MotionLimits,
}

impl NvidiaMotion {
    /// The part, where NVENC's H.264 encoder on `gpu` has an ME-only mode.
    pub fn new(gpu: Arc<Gpu>) -> Result<Self, Error> {
        let limits = motion_limits(Arc::clone(&gpu))?;
        Ok(Self { gpu, limits })
    }

    /// The frame NVENC estimates a `width` by `height` picture in: the
    /// picture's size made even, and at least NVENC's least.
    fn frame_size(&self, width: u32, height: u32) -> (u32, u32) {
        (
            width
                .next_multiple_of(2)
                .max(self.limits.min_width.next_multiple_of(2)),
            height
                .next_multiple_of(2)
                .max(self.limits.min_height.next_multiple_of(2)),
        )
    }
}

impl MotionEstimationBackend for NvidiaMotion {
    fn capabilities(&self) -> MotionEstimationCapabilities {
        MotionEstimationCapabilities {
            source_formats: vec![PixelFormat::Nv12VideoRange, PixelFormat::Nv12FullRange],
            max_width: self.limits.max_width.min(MAX_SIDE),
            max_height: self.limits.max_height.min(MAX_SIDE),
        }
    }

    fn open(
        &self,
        config: &MotionEstimatorConfig,
    ) -> Result<Box<dyn MotionEstimator>, BackendError> {
        let (width, height) = self.frame_size(config.width, config.height);
        if width > self.limits.max_width || height > self.limits.max_height {
            return Err(BackendError::Unsupported(format!(
                "NVENC estimates motion up to {}x{}, not {}x{}",
                self.limits.max_width, self.limits.max_height, config.width, config.height
            )));
        }
        let motion = NvencMotion::new(Arc::clone(&self.gpu), width, height, PRESET)
            .map_err(|error| BackendError::Unsupported(error.to_string()))?;
        Ok(Box::new(NvidiaEstimator {
            motion,
            config: *config,
            frame: (width, height),
            reference: Frame::default(),
            current: Frame::default(),
            searched: false,
        }))
    }
}

/// An NV12 frame of the session's size, the picture's planes copied in.
#[derive(Default)]
struct Frame {
    luma: Vec<u8>,
    chroma: Vec<u8>,
}

struct NvidiaEstimator {
    motion: NvencMotion,
    config: MotionEstimatorConfig,
    /// The frame NVENC estimates in.
    frame: (u32, u32),
    reference: Frame,
    current: Frame,
    /// Whether NVENC has estimated a pair on this session yet.
    searched: bool,
}

impl MotionEstimator for NvidiaEstimator {
    fn estimate(
        &mut self,
        reference: &MotionPicture<'_>,
        current: &MotionPicture<'_>,
    ) -> Result<MotionField, BackendError> {
        for picture in [reference, current] {
            if picture.pixel_format.bytes_per_sample() != 1 {
                return Err(BackendError::Unsupported(format!(
                    "NVENC's motion estimation takes NV12, not {:?}",
                    picture.pixel_format
                )));
            }
        }
        let (width, height) = (self.config.width, self.config.height);
        fill(
            &mut self.reference,
            reference.planes,
            (width, height),
            self.frame,
        )?;
        fill(
            &mut self.current,
            current.planes,
            (width, height),
            self.frame,
        )?;
        let row = usize_of(self.frame.0);
        let (reference, current) = (nv12(&self.reference, row), nv12(&self.current, row));
        let malfunction = |error: Error| BackendError::Malfunction(error.to_string());
        if !self.searched {
            // Ours: a session's first pair is estimated twice, and the
            // first answer dropped. NVENC searches from its last answer
            // too, and a session's first search has none: on 256x128 moved
            // by (20, -12), the first estimate gave the way back to 62.5%
            // of Apple's 16x16 cells and the same pair's second 92.2%
            // (RTX 5080 Laptop GPU, 2026-10-24, `gpu::measure_presets`). It costs
            // one estimate a session.
            self.motion
                .estimate(&reference, &current)
                .map_err(malfunction)?;
            self.searched = true;
        }
        let macroblocks = self
            .motion
            .estimate(&reference, &current)
            .map_err(malfunction)?;
        Ok(field(
            &macroblocks,
            self.frame.0.div_ceil(16),
            width,
            height,
        ))
    }
}

/// NVENC's view of `frame`, `row` bytes a row in each plane.
fn nv12(frame: &Frame, row: usize) -> Nv12<'_> {
    Nv12 {
        luma: (&frame.luma, row),
        chroma: (&frame.chroma, row),
    }
}

/// Copies a `picture`-sized NV12 picture's planes into `frame`, of the
/// session's size `frame_size`, repeating the picture's last column and
/// row, and its chroma's, into the rest.
fn fill(
    frame: &mut Frame,
    planes: &[PlaneSource<'_>],
    (width, height): (u32, u32),
    (frame_width, frame_height): (u32, u32),
) -> Result<(), BackendError> {
    let [luma, chroma] = planes else {
        return Err(BackendError::BadData(format!(
            "an NV12 picture of {} planes",
            planes.len()
        )));
    };
    let (frame_width, frame_height) = (usize_of(frame_width), usize_of(frame_height));
    frame.luma.resize(frame_width * frame_height, 0);
    frame.chroma.resize(frame_width * frame_height / 2, 0);
    // Luma: one sample a pixel; chroma: one CbCr pair a 2x2 block.
    copy_padded(
        &mut frame.luma,
        frame_width,
        frame_height,
        luma,
        usize_of(width),
        usize_of(height),
        1,
    )?;
    copy_padded(
        &mut frame.chroma,
        frame_width,
        frame_height / 2,
        chroma,
        usize_of(width.div_ceil(2)),
        usize_of(height.div_ceil(2)),
        2,
    )
}

/// Copies `rows` rows of `columns` samples of `sample` bytes from `source`
/// into the top left of `destination`, `pitch` bytes a row and
/// `destination_rows` rows, and repeats the last sample of each row and
/// the last row into the rest.
fn copy_padded(
    destination: &mut [u8],
    pitch: usize,
    destination_rows: usize,
    source: &PlaneSource<'_>,
    columns: usize,
    rows: usize,
    sample: usize,
) -> Result<(), BackendError> {
    let row_bytes = columns * sample;
    if rows == 0 || row_bytes == 0 {
        return Err(BackendError::BadData("an empty picture".into()));
    }
    let needed = (rows - 1)
        .checked_mul(source.bytes_per_row)
        .and_then(|bytes| bytes.checked_add(row_bytes));
    if source.bytes_per_row < row_bytes
        || usize_of(source.rows) < rows
        || needed.is_none_or(|needed| source.bytes.len() < needed)
    {
        return Err(BackendError::BadData(
            "a plane that does not hold the picture".into(),
        ));
    }
    for row in 0..destination_rows {
        let from_row = row.min(rows - 1);
        let from = &source.bytes[from_row * source.bytes_per_row..][..row_bytes];
        let to = &mut destination[row * pitch..(row + 1) * pitch];
        to[..row_bytes].copy_from_slice(from);
        let last = &from[row_bytes - sample..];
        for padding in to[row_bytes..].chunks_exact_mut(sample) {
            padding.copy_from_slice(last);
        }
    }
    Ok(())
}

/// The field of the picture's 8x8 blocks from NVENC's macroblocks,
/// `macroblock_columns` a row of the frame they were estimated in.
fn field(
    macroblocks: &[NV_ENC_H264_MV_DATA],
    macroblock_columns: u32,
    width: u32,
    height: u32,
) -> MotionField {
    let (columns, rows) = (width.div_ceil(BLOCK), height.div_ceil(BLOCK));
    let mut vectors = Vec::with_capacity(usize_of(columns).saturating_mul(usize_of(rows)));
    for row in 0..rows {
        for column in 0..columns {
            let index = usize_of(row / 2) * usize_of(macroblock_columns) + usize_of(column / 2);
            let vector = macroblocks
                .get(index)
                .map(|macroblock| partition_vector(macroblock, column % 2, row % 2))
                .unwrap_or_default();
            vectors.push(MotionVector {
                x: f32::from(vector.mvx) / 4.0,
                y: f32::from(vector.mvy) / 4.0,
            });
        }
    }
    MotionField {
        block_size: BLOCK,
        columns,
        rows,
        vectors,
    }
}

/// The vector of the 8x8 block at (`column`, `row`), each 0 or 1, of a
/// macroblock: NVENC gives each of the four its own, in raster order,
/// whatever the partition. Measured on an RTX 5080 Laptop GPU (driver
/// 615.71.09, 2026-10-24, `gpu::print_what_nvenc_answers`): a 16x16
/// macroblock held four equal vectors, a 16x8 one the top's twice then the
/// bottom's twice, and an 8x16 one the left's and the right's, then both
/// again.
fn partition_vector(macroblock: &NV_ENC_H264_MV_DATA, column: u32, row: u32) -> NV_ENC_MVECTOR {
    (macroblock.mv.get(usize_of(row) * 2 + usize_of(column)))
        .copied()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn macroblock(partition: u8, mv: [(i16, i16); 4]) -> NV_ENC_H264_MV_DATA {
        NV_ENC_H264_MV_DATA {
            mv: mv.map(|(mvx, mvy)| NV_ENC_MVECTOR { mvx, mvy }),
            mbType: 1,
            partitionType: partition,
            ..Default::default()
        }
    }

    /// Each 8x8 block takes its own of the macroblock's vectors, in
    /// pixels, whatever the partition, as NVENC lays them out.
    #[test]
    fn partitions_spread_over_their_blocks_in_pixels() {
        let read = |partition, vectors| {
            let field = field(&[macroblock(partition, vectors)], 1, 16, 16);
            field
                .vectors
                .iter()
                .map(|vector| (vector.x, vector.y))
                .collect::<Vec<_>>()
        };
        assert_eq!(read(0, [(4, 8); 4]), [(1.0, 2.0); 4]);
        assert_eq!(
            read(1, [(4, 8), (-4, 2), (1, -1), (0, 6)]),
            [(1.0, 2.0), (-1.0, 0.5), (0.25, -0.25), (0.0, 1.5)]
        );
        assert_eq!(
            read(2, [(-24, 0), (-24, 0), (24, 0), (24, 0)]),
            [(-6.0, 0.0), (-6.0, 0.0), (6.0, 0.0), (6.0, 0.0)]
        );
        assert_eq!(
            read(3, [(0, -24), (0, 24), (0, -24), (0, 24)]),
            [(0.0, -6.0), (0.0, 6.0), (0.0, -6.0), (0.0, 6.0)]
        );
    }

    /// A picture smaller than the frame keeps only its own blocks, across
    /// the frame's macroblock rows.
    #[test]
    fn the_picture_keeps_its_own_blocks() {
        let macroblocks: Vec<_> = (0..6).map(|n| macroblock(0, [(n * 4, 0); 4])).collect();
        let field = field(&macroblocks, 3, 20, 20);
        assert_eq!((field.columns, field.rows), (3, 3));
        let xs: Vec<f32> = field.vectors.iter().map(|vector| vector.x).collect();
        assert_eq!(xs, [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 3.0, 3.0, 4.0]);
    }

    /// The padding repeats the last column and row, of luma and chroma.
    #[test]
    fn a_small_picture_is_padded_with_its_edges() -> Result<(), BackendError> {
        let luma = [1, 2, 3, 4, 5, 6];
        let chroma = [7, 8, 9, 10];
        let planes = [
            PlaneSource {
                bytes: &luma,
                bytes_per_row: 3,
                rows: 2,
            },
            PlaneSource {
                bytes: &chroma,
                bytes_per_row: 4,
                rows: 1,
            },
        ];
        let mut frame = Frame::default();
        fill(&mut frame, &planes, (3, 2), (6, 4))?;
        assert_eq!(&frame.luma[..6], [1, 2, 3, 3, 3, 3]);
        assert_eq!(&frame.luma[18..], [4, 5, 6, 6, 6, 6]);
        assert_eq!(&frame.chroma[..6], [7, 8, 9, 10, 9, 10]);
        assert_eq!(&frame.chroma[6..], [7, 8, 9, 10, 9, 10]);
        Ok(())
    }
}

/// What NVENC answers on this host's GPU, run there: `cargo test -p
/// reims-vgpu-video-nvidia --lib motion::gpu -- --ignored --nocapture`.
#[cfg(test)]
mod gpu {
    use super::*;
    use crate::cuda::GpuSelector;
    use reims_vgpu_video_conformance::motion::texture::moved_luma;
    use reims_vgpu_video_core::motion::{AppleGrid, BlockSize, resample};
    use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
    use std::time::Duration;

    /// The vectors NVENC gives `current` against a still texture.
    fn estimate(
        width: usize,
        height: usize,
        current: &[u8],
        preset: u8,
    ) -> Result<Vec<NV_ENC_H264_MV_DATA>, Error> {
        let gpu = Arc::new(Gpu::open(&GpuSelector::parse(""))?);
        let mut motion = NvencMotion::new(gpu, width as u32, height as u32, preset)?;
        let chroma = vec![128; width * height / 2];
        let still = moved_luma(width, height, width, 0.0, 0.0);
        motion.estimate(
            &Nv12 {
                luma: (&still, width),
                chroma: (&chroma, width),
            },
            &Nv12 {
                luma: (current, width),
                chroma: (&chroma, width),
            },
        )
    }

    /// A texture whose rows above `row`, or columns left of `column`, moved
    /// by `first`, and the rest by `second`.
    fn split(
        (width, height): (usize, usize),
        (row, column): (usize, usize),
        first: (f64, f64),
        second: (f64, f64),
    ) -> Vec<u8> {
        let (a, b) = (
            moved_luma(width, height, width, first.0, first.1),
            moved_luma(width, height, width, second.0, second.1),
        );
        (0..width * height)
            .map(|i| {
                if i / width < row || i % width < column {
                    a[i]
                } else {
                    b[i]
                }
            })
            .collect()
    }

    fn summary(macroblocks: &[NV_ENC_H264_MV_DATA], back: (i16, i16)) -> String {
        let mut types = std::collections::BTreeMap::new();
        let mut hits = 0;
        for macroblock in macroblocks {
            *types
                .entry((macroblock.mbType, macroblock.partitionType))
                .or_insert(0) += 1;
            hits += macroblock
                .mv
                .iter()
                .filter(|vector| (vector.mvx, vector.mvy) == back)
                .count();
        }
        format!(
            "{} macroblocks, (type, partition) {types:?}, {hits} of {} vectors are {back:?} \
             quarter pixels",
            macroblocks.len(),
            macroblocks.len() * 4
        )
    }

    fn vectors(macroblock: &NV_ENC_H264_MV_DATA) -> String {
        let vectors: Vec<_> = macroblock.mv.iter().map(|v| (v.mvx, v.mvy)).collect();
        format!(
            "type {} partition {} cost {} {vectors:?}",
            macroblock.mbType, macroblock.partitionType, macroblock.mbCost
        )
    }

    /// Prints the macroblocks' types, partitions and vectors for a moved
    /// texture, and for textures whose halves moved apart inside a row or a
    /// column of macroblocks.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn print_what_nvenc_answers() -> Result<(), Error> {
        let limits = motion_limits(Arc::new(Gpu::open(&GpuSelector::parse(""))?))?;
        println!("limits {limits:?}");
        let (width, height) = (256, 128);
        for (shift, preset) in [
            ((8.0, 4.0), 4),
            ((-5.0, 7.0), 4),
            ((2.5, -1.25), 4),
            ((8.0, 4.0), 7),
        ] {
            let current = moved_luma(width, height, width, shift.0, shift.1);
            let macroblocks = estimate(width, height, &current, preset)?;
            let back = (-(shift.0 * 4.0) as i16, -(shift.1 * 4.0) as i16);
            println!("shift {shift:?} P{preset}: {}", summary(&macroblocks, back));
            for macroblock in macroblocks.iter().skip(17).take(2) {
                println!("  {}", vectors(macroblock));
            }
        }
        // Row 72 is inside macroblock row 4, column 72 inside column 4.
        let columns = width / 16;
        for (name, boundary, first, second) in [
            ("rows", (72, 0), (6.0, 0.0), (-6.0, 0.0)),
            ("columns", (0, 72), (0.0, 6.0), (0.0, -6.0)),
        ] {
            let current = split((width, height), boundary, first, second);
            let macroblocks = estimate(width, height, &current, 4)?;
            println!("{name} split, {first:?} then {second:?}:");
            let along: Vec<usize> = if name == "rows" {
                (4 * columns + 2..4 * columns + 6).collect()
            } else {
                (2..6).map(|row| row * columns + 4).collect()
            };
            for index in along {
                println!("  macroblock {index}: {}", vectors(&macroblocks[index]));
            }
        }
        Ok(())
    }

    /// An NV12 picture of the conformance texture moved by a shift, its
    /// chroma grey.
    struct Picture {
        luma: Vec<u8>,
        chroma: Vec<u8>,
        width: usize,
    }

    impl Picture {
        fn new(width: usize, height: usize, (dx, dy): (f64, f64)) -> Self {
            Self {
                luma: moved_luma(width, height, width, dx, dy),
                chroma: vec![128; width * height.div_ceil(2)],
                width,
            }
        }

        fn planes(&self) -> [PlaneSource<'_>; 2] {
            [
                PlaneSource {
                    bytes: &self.luma,
                    bytes_per_row: self.width,
                    rows: (self.luma.len() / self.width) as u32,
                },
                PlaneSource {
                    bytes: &self.chroma,
                    bytes_per_row: self.width,
                    rows: (self.chroma.len() / self.width) as u32,
                },
            ]
        }
    }

    /// The backend's estimator for `width` by `height` pictures, at preset
    /// P`preset` whatever the guest asked.
    fn estimator(
        motion: &NvidiaMotion,
        (width, height): (u32, u32),
        preset: u8,
    ) -> Result<NvidiaEstimator, Error> {
        let frame = motion.frame_size(width, height);
        Ok(NvidiaEstimator {
            motion: NvencMotion::new(Arc::clone(&motion.gpu), frame.0, frame.1, preset)?,
            config: MotionEstimatorConfig {
                width,
                height,
                block: BlockSize::Sixteen,
                true_motion: false,
            },
            frame,
            reference: Frame::default(),
            current: Frame::default(),
            // NVENC's own answers, each pair estimated once.
            searched: true,
        })
    }

    fn estimate_pair(
        estimator: &mut dyn MotionEstimator,
        reference: &Picture,
        current: &Picture,
    ) -> Result<MotionField, Box<dyn std::error::Error>> {
        let picture = |planes| MotionPicture {
            pixel_format: PixelFormat::Nv12VideoRange,
            planes,
        };
        let (reference, current) = (reference.planes(), current.planes());
        Ok(estimator.estimate(&picture(&reference), &picture(&current))?)
    }

    /// The share of `vectors`, `columns` a row, in the top left `width` by
    /// `height` of them, within 2 pixels of `back` each way.
    fn share(
        vectors: &[MotionVector],
        columns: u32,
        (width, height): (u32, u32),
        back: (f32, f32),
    ) -> f64 {
        let mut hits = 0_u32;
        for row in 0..height {
            for column in 0..width {
                let vector = vectors[(row * columns + column) as usize];
                hits +=
                    u32::from((vector.x - back.0).abs() <= 2.0 && (vector.y - back.1).abs() <= 2.0);
            }
        }
        f64::from(hits) / f64::from(width * height)
    }

    /// Each preset's hits and time per estimate, through the backend's
    /// estimator.
    ///
    /// Hits: frames K of the conformance texture moved by K times (20, -12),
    /// three pairs (K, K + 1) on one session, as vtmotion sends them; for
    /// each, the share of the picture's 8x8 blocks, and of its 16x16 cells
    /// on Apple's grid as the guest gets them, within 2 pixels of the way
    /// back. Time: from the call to the field, the copies in included, of
    /// 100 estimates after 10, at 720p, 1080p and 4K.
    #[test]
    #[ignore = "needs an NVIDIA GPU: run on the NVIDIA host"]
    fn measure_presets() -> Result<(), Box<dyn std::error::Error>> {
        let motion = NvidiaMotion::new(Arc::new(Gpu::open(&GpuSelector::parse(""))?))?;
        let shift = (20.0, -12.0);
        let back = (-20.0, 12.0);
        for size in [(256, 128), (1280, 720)] {
            let (width, height) = (size.0 as usize, size.1 as usize);
            let pictures: Vec<Picture> = (0..4)
                .map(|k| Picture::new(width, height, (shift.0 * k as f64, shift.1 * k as f64)))
                .collect();
            let grid = AppleGrid {
                width: size.0,
                height: size.1,
                block: BlockSize::Sixteen,
            };
            let hits = |field: &MotionField| {
                let blocks = share(
                    &field.vectors,
                    field.columns,
                    (field.columns, field.rows),
                    back,
                );
                let cells = share(
                    &resample(field, &grid),
                    grid.columns(),
                    (size.0.div_ceil(16), size.1.div_ceil(16)),
                    back,
                );
                format!(
                    "{:.1}% of 8x8, {:.1}% of 16x16",
                    blocks * 100.0,
                    cells * 100.0
                )
            };
            // vtmotion's order, then the first pair again and again, then
            // the last pair first: whether a session's history helps.
            for (order, pairs) in [
                ("pairs 0 1 2", [0, 1, 2]),
                ("pair 0 thrice", [0, 0, 0]),
                ("pairs 2 0 1", [2, 0, 1]),
            ] {
                for preset in 1..=7 {
                    let mut estimator = estimator(&motion, size, preset)?;
                    let mut line = format!("hits {}x{} P{preset}, {order}:", size.0, size.1);
                    for k in pairs {
                        let field = estimate_pair(&mut estimator, &pictures[k], &pictures[k + 1])?;
                        line += &format!(" pair {k} {};", hits(&field));
                    }
                    println!("{line}");
                }
            }
            // The backend as a guest's session opens it.
            let mut opened = motion.open(&MotionEstimatorConfig {
                width: size.0,
                height: size.1,
                block: BlockSize::Sixteen,
                true_motion: false,
            })?;
            let mut line = format!("hits {}x{} as opened, pairs 0 1 2:", size.0, size.1);
            for k in 0..3 {
                let start = StdRuntime.now();
                let field = estimate_pair(&mut *opened, &pictures[k], &pictures[k + 1])?;
                line += &format!(
                    " pair {k} {} in {:.2} ms;",
                    hits(&field),
                    StdRuntime.elapsed_since(start).as_secs_f64() * 1000.0
                );
            }
            println!("{line}");
        }
        for size in [(1280, 720), (1920, 1080), (3840, 2160)] {
            let (width, height) = (size.0 as usize, size.1 as usize);
            let reference = Picture::new(width, height, (0.0, 0.0));
            let current = Picture::new(width, height, shift);
            for preset in [1, 4, 5, 6, 7] {
                let mut estimator = estimator(&motion, size, preset)?;
                for _ in 0..10 {
                    estimate_pair(&mut estimator, &reference, &current)?;
                }
                let mut times: Vec<Duration> = (0..100)
                    .map(|_| {
                        let start = StdRuntime.now();
                        estimate_pair(&mut estimator, &reference, &current)
                            .map(|_| StdRuntime.elapsed_since(start))
                    })
                    .collect::<Result<_, _>>()?;
                times.sort();
                let ms = |time: Duration| time.as_secs_f64() * 1000.0;
                let mean = times.iter().map(|&time| ms(time)).sum::<f64>() / times.len() as f64;
                println!(
                    "time {}x{} P{preset}: median {:.2} ms, mean {mean:.2}, 95th {:.2}, most {:.2}",
                    size.0,
                    size.1,
                    ms(times[50]),
                    ms(times[95]),
                    ms(times[99]),
                );
            }
        }
        Ok(())
    }
}
